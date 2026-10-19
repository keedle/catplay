//! Reuse the existing PCM audio bridge with explicit per-stream ownership.
use super::{MirrorSessionId, TxAdapterOp};
use catplay_carplay::{
    audio::{AudioPlayer, AudioPlayerBox, AudioSourceBox, AudioStreamBasicDescription},
    msg::{AudioFormat, AudioType, InfoMessageResponse, StreamType},
    rtsp_frame::{RtspError, RtspResult},
};
use catplay_util::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A separate token for each audio SETUP, including repeated SETUP in one RTSP session.
#[derive(Clone, Debug)]
pub struct MirrorAudioAttempt(Arc<AtomicBool>);
impl MirrorAudioAttempt {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
    pub fn is_active(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn retire(&self) {
        self.0.store(false, Ordering::Release);
    }
    pub fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

fn pcm_mask() -> AudioFormat {
    AudioFormat::PCM_8000_MONO
        | AudioFormat::PCM_8000_STEREO
        | AudioFormat::PCM_16000_MONO
        | AudioFormat::PCM_16000_STEREO
        | AudioFormat::PCM_24000_MONO
        | AudioFormat::PCM_24000_STEREO
        | AudioFormat::PCM_32000_MONO
        | AudioFormat::PCM_32000_STEREO
        | AudioFormat::PCM_44100_MONO
        | AudioFormat::PCM_44100_STEREO
        | AudioFormat::PCM_48000_MONO
        | AudioFormat::PCM_48000_STEREO
}

/// Only live car MainAudio media output formats participate. No resampler is
/// present: decoded rate/channel/sample representation must match exactly.
pub fn mirror_car_pcm_formats(info: &InfoMessageResponse) -> AudioFormat {
    let mut formats = AudioFormat::empty();
    let mut found_media = false;
    for entry in &info.audio_formats {
        if entry.stream_type == StreamType::MainAudio && matches!(entry.audio_type, None | Some(AudioType::Media)) {
            found_media = true;
            formats |= entry.audio_output_formats.unwrap_or_default();
        }
    }
    if !found_media {
        for entry in &info.audio_formats {
            if entry.stream_type == StreamType::MainAudio && entry.audio_type == Some(AudioType::Compatibility) {
                formats |= entry.audio_output_formats.unwrap_or_default();
            }
        }
    }
    formats & pcm_mask()
}

pub fn mirror_pcm_supported(allowed: AudioFormat, pcm: AudioStreamBasicDescription) -> bool {
    if pcm.channels_per_frame == 0 || pcm.channels_per_frame > 2 {
        return false;
    }
    let expected = AudioStreamBasicDescription::fill_pcm(pcm.sample_rate, 16, 16, pcm.channels_per_frame as u8, false);
    pcm.format == expected.format
        && pcm.format_flags == expected.format_flags
        && pcm.bits_per_channel == expected.bits_per_channel
        && pcm.bytes_per_frame == expected.bytes_per_frame
        && pcm.bytes_per_packet == expected.bytes_per_packet
        && pcm.frames_per_packet == expected.frames_per_packet
        && AudioFormat::try_from(pcm).is_ok_and(|format| !format.is_empty() && allowed.contains(format))
}

/// Existing AAC/ALAC decoders output signed interleaved i16 PCM. Advertise
/// compressed input only when that exact decoded format is supported by the car.
pub fn mirror_input_audio_formats(car_pcm: AudioFormat) -> AudioFormat {
    let mut formats = car_pcm & pcm_mask();
    for input in [
        AudioFormat::ALAC_44100_16_STEREO,
        AudioFormat::ALAC_44100_24_STEREO,
        AudioFormat::ALAC_48000_16_STEREO,
        AudioFormat::ALAC_48000_24_STEREO,
        AudioFormat::AAC_LC_44100_STEREO,
        AudioFormat::AAC_LC_48000_STEREO,
        AudioFormat::AAC_ELD_16000_MONO,
        AudioFormat::AAC_ELD_24000_MONO,
        AudioFormat::AAC_ELD_44100_STEREO,
        AudioFormat::AAC_ELD_48000_STEREO,
        AudioFormat::AAC_ELD_44100_MONO,
        AudioFormat::AAC_ELD_48000_MONO,
    ] {
        if let Some(source) = input.as_asbd() {
            let decoded = AudioStreamBasicDescription::fill_pcm(source.sample_rate, 16, 16, source.channels_per_frame as u8, false);
            if mirror_pcm_supported(car_pcm, decoded) {
                formats |= input;
            }
        }
    }
    formats
}

/// A stale async SETUP result must be destroyed and its car TEARDOWN drained
/// before the next queued attempt can run, even within the same RTSP session.
pub(crate) async fn complete_mirror_audio_setup<T, F, C, CF>(
    session: &MirrorSessionId,
    attempt: &MirrorAudioAttempt,
    setup: F,
    cleanup: C,
) -> RtspResult<T>
where
    T: Send,
    F: std::future::Future<Output = RtspResult<T>> + Send,
    C: FnOnce() -> CF + Send,
    CF: std::future::Future<Output = RtspResult<()>> + Send,
{
    if !session.is_active() || !attempt.is_active() {
        return Err(RtspError::Closed);
    }
    let result = setup.await?;
    if !session.is_active() || !attempt.is_active() {
        drop(result);
        cleanup().await?;
        return Err(RtspError::Closed);
    }
    Ok(result)
}

pub(crate) struct MirrorAudioPlayer {
    inner: Option<AudioPlayerBox<i16>>,
    session: MirrorSessionId,
    attempt: MirrorAudioAttempt,
    car: mpsc::UnboundedSender<TxAdapterOp>,
}
impl MirrorAudioPlayer {
    pub fn new(
        inner: AudioPlayerBox<i16>,
        session: MirrorSessionId,
        attempt: MirrorAudioAttempt,
        car: mpsc::UnboundedSender<TxAdapterOp>,
    ) -> Self {
        Self {
            inner: Some(inner),
            session,
            attempt,
            car,
        }
    }
}
impl AudioPlayer for MirrorAudioPlayer {
    type Sample = i16;
    fn init(&mut self, source: AudioSourceBox<i16>) -> RtspResult<()> {
        if !self.session.is_active() || !self.attempt.is_active() {
            return Err(RtspError::Closed);
        }
        self.inner.as_mut().ok_or(RtspError::Closed)?.init(source)
    }
    fn start(&mut self) {
        if self.session.is_active() && self.attempt.is_active() {
            if let Some(inner) = self.inner.as_mut() {
                inner.start();
            }
        }
    }
    fn stop(&mut self, drain: bool) {
        // RTP underflow can pause/resume the same stream. Only Drop retires it.
        if let Some(inner) = self.inner.as_mut() {
            inner.stop(drain);
        }
    }
}
impl Drop for MirrorAudioPlayer {
    fn drop(&mut self) {
        self.attempt.retire();
        if let Some(mut inner) = self.inner.take() {
            inner.stop(false);
            drop(inner);
        }
        // The inner car teardown guard has already been dropped before release.
        let _ = self.car.unbounded_send(TxAdapterOp::MirrorAudioEnded {
            id: self.session.clone(),
            attempt: self.attempt.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_carplay::msg::AudioFormatStruct;
    #[test]
    fn live_media_pcm_intersection_excludes_other_types_rates_and_microphone() {
        let mut info = InfoMessageResponse::default();
        info.audio_formats = vec![
            AudioFormatStruct {
                stream_type: StreamType::MainAudio,
                audio_type: Some(AudioType::Media),
                audio_output_formats: Some(AudioFormat::PCM_48000_STEREO),
                audio_input_formats: Some(AudioFormat::PCM_44100_STEREO),
            },
            AudioFormatStruct {
                stream_type: StreamType::AltAudio,
                audio_type: None,
                audio_output_formats: Some(AudioFormat::PCM_44100_STEREO),
                audio_input_formats: None,
            },
        ];
        let car = mirror_car_pcm_formats(&info);
        assert_eq!(car, AudioFormat::PCM_48000_STEREO);
        let inputs = mirror_input_audio_formats(car);
        assert!(inputs.contains(AudioFormat::AAC_LC_48000_STEREO | AudioFormat::ALAC_48000_16_STEREO));
        assert!(!inputs.intersects(AudioFormat::AAC_LC_44100_STEREO | AudioFormat::PCM_44100_STEREO));
        assert!(mirror_pcm_supported(
            car,
            AudioStreamBasicDescription::fill_pcm(48000, 16, 16, 2, false)
        ));
        assert!(!mirror_pcm_supported(
            car,
            AudioStreamBasicDescription::fill_pcm(44100, 16, 16, 2, false)
        ));
        assert!(!mirror_pcm_supported(
            car,
            AudioStreamBasicDescription::fill_pcm(48000, 32, 32, 2, true)
        ));
    }
    #[tokio::test]
    async fn stale_same_session_setup_drops_guard_then_drains_before_successor() {
        use std::sync::Mutex;
        struct Guard(Arc<Mutex<Vec<&'static str>>>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.lock().unwrap().push("drop guard");
            }
        }
        let session = MirrorSessionId::new();
        let old = MirrorAudioAttempt::new();
        let successor = MirrorAudioAttempt::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let task_session = session.clone();
        let task_attempt = old.clone();
        let setup_events = events.clone();
        let cleanup_events = events.clone();
        let task = tokio::spawn(async move {
            complete_mirror_audio_setup(
                &task_session,
                &task_attempt,
                async {
                    ready_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    Ok(Guard(setup_events))
                },
                || async {
                    cleanup_events.lock().unwrap().push("drain teardown");
                    Ok(())
                },
            )
            .await
        });
        ready_rx.await.unwrap();
        old.retire(); // TEARDOWN while car SETUP is pending, same RTSP owner.
        finish_tx.send(()).unwrap();
        assert!(matches!(task.await.unwrap(), Err(RtspError::Closed)));
        assert_eq!(*events.lock().unwrap(), vec!["drop guard", "drain teardown"]);
        assert!(session.is_active());
        assert!(successor.is_active());
        let value = complete_mirror_audio_setup(&session, &successor, async { Ok(42) }, || async {
            panic!("current successor must not be cleaned up");
        })
        .await
        .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn retired_attempt_never_polls_setup() {
        let session = MirrorSessionId::new();
        let attempt = MirrorAudioAttempt::new();
        attempt.retire();
        let result = complete_mirror_audio_setup::<(), _, _, _>(
            &session,
            &attempt,
            async { panic!("must not allocate an obsolete stream") },
            || async { panic!("nothing allocated") },
        )
        .await;
        assert!(matches!(result, Err(RtspError::Closed)));
    }

    #[test]
    fn audio_pause_preserves_attempt_but_drop_retires_and_drops_before_release() {
        use std::sync::Mutex;
        struct Player(Arc<Mutex<Vec<&'static str>>>);
        impl AudioPlayer for Player {
            type Sample = i16;
            fn init(&mut self, _: AudioSourceBox<i16>) -> RtspResult<()> {
                Ok(())
            }
            fn start(&mut self) {
                self.0.lock().unwrap().push("start");
            }
            fn stop(&mut self, _: bool) {
                self.0.lock().unwrap().push("stop");
            }
        }
        impl Drop for Player {
            fn drop(&mut self) {
                self.0.lock().unwrap().push("drop car guard");
            }
        }
        let session = MirrorSessionId::new();
        let attempt = MirrorAudioAttempt::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = mpsc::unbounded();
        let mut player = MirrorAudioPlayer::new(Box::new(Player(events.clone())), session.clone(), attempt.clone(), tx);
        player.start();
        player.stop(false);
        assert!(attempt.is_active());
        assert!(rx.try_next().is_err());
        player.start();
        drop(player);
        assert!(!attempt.is_active());
        assert_eq!(*events.lock().unwrap(), vec!["start", "stop", "start", "stop", "drop car guard"]);
        assert!(
            matches!(rx.try_next(), Ok(Some(TxAdapterOp::MirrorAudioEnded { id, attempt: released })) if id.matches(&session) && released.matches(&attempt))
        );
    }

    #[test]
    fn audio_attempts_are_unique_even_within_one_session() {
        let first = MirrorAudioAttempt::new();
        let successor = MirrorAudioAttempt::new();
        first.retire();
        assert!(!first.is_active());
        assert!(successor.is_active());
        assert!(!first.matches(&successor));
    }
}
