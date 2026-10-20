//! Mirroring callbacks reuse media paths while keeping phone controls separate.
use crate::{
    MirroringOptions,
    proxy_rpc::{MirrorBridge, MirrorLimits, TxAdapterOp},
};
use async_trait::async_trait;
use catplay_carplay::{
    carplay_rx::AIRPLAY_MIRRORING_MAX_SCREEN_PAYLOAD,
    carplay_rx::{AirPlayInitialSetupObservation, AirPlayReceiverDiagnostic, AirPlayReceiverHandleRef, AirPlayReceiverSink},
    modes::ChangeModes,
    msg::{Command, Display, InfoMessageResponse},
    rtsp_frame::{RtspError, RtspResponse, RtspResult},
    screen::rx::ScreenReceiverSinkBox,
};
use catplay_util::{AsyncShutdown, EventReconciler, EventSleeper, mpsc};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// A retained identity token cannot collide with an old or foreign session.
#[derive(Clone, Debug)]
pub struct MirrorSessionId(Arc<AtomicBool>);
impl MirrorSessionId {
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

pub struct MirrorRxSession {
    car: mpsc::UnboundedSender<TxAdapterOp>,
    id: MirrorSessionId,
    options: MirroringOptions,
    width: u32,
    height: u32,
    handle: Option<AirPlayReceiverHandleRef>,
    screen_status: Option<super::MirrorStreamStatus>,
    car_pcm_formats: catplay_carplay::msg::AudioFormat,
    audio_attempt: Option<super::MirrorAudioAttempt>,
    diagnostic_events: u8,
    connection_ended_reported: bool,
}
impl MirrorRxSession {
    pub fn new(car: mpsc::UnboundedSender<TxAdapterOp>, options: MirroringOptions, width: u32, height: u32) -> Self {
        Self {
            car,
            id: MirrorSessionId::new(),
            options,
            width,
            height,
            handle: None,
            screen_status: None,
            car_pcm_formats: catplay_carplay::msg::AudioFormat::empty(),
            audio_attempt: None,
            diagnostic_events: 0,
            connection_ended_reported: false,
        }
    }
    pub fn with_audio_formats(mut self, car_pcm_formats: catplay_carplay::msg::AudioFormat) -> Self {
        self.car_pcm_formats = car_pcm_formats;
        self
    }
    fn finish(&mut self) {
        if self.handle.is_some() {
            #[cfg(feature = "experimental_airplay_mirroring")]
            eprintln!("mirror_lifecycle stage=closed");
        }
        self.id.retire();
        if let Some(attempt) = self.audio_attempt.take() {
            attempt.retire();
        }
        if self.handle.take().is_some() {
            let _ = self
                .car
                .unbounded_send(TxAdapterOp::MirrorDisconnected { id: self.id.clone() });
        }
    }
}
#[async_trait]
impl AirPlayReceiverSink for MirrorRxSession {
    fn init(&mut self, handle: AirPlayReceiverHandleRef) -> RtspResult<()> {
        if self.car.is_closed() || self.width == 0 || self.height == 0 || handle.is_car() || !handle.is_mirroring() || self.handle.is_some()
        {
            return Err(RtspError::NotSupported);
        }
        self.car
            .unbounded_send(TxAdapterOp::MirrorConnected {
                id: self.id.clone(),
                handle: handle.clone(),
            })
            .map_err(|_| RtspError::Closed)?;
        self.handle = Some(handle);
        #[cfg(feature = "experimental_airplay_mirroring")]
        eprintln!("mirror_lifecycle stage=admitted");
        Ok(())
    }
    fn on_receiver_diagnostic(&mut self, diagnostic: AirPlayReceiverDiagnostic) {
        if let AirPlayReceiverDiagnostic::ConnectionEnded(class) = diagnostic {
            if !self.connection_ended_reported {
                self.connection_ended_reported = true;
                #[cfg(feature = "experimental_airplay_mirroring")]
                eprintln!("mirror_lifecycle stage=connection_ended class={class:?}");
            }
            return;
        }
        if self.diagnostic_events >= 32 {
            return;
        }
        self.diagnostic_events += 1;
        #[cfg(feature = "experimental_airplay_mirroring")]
        match diagnostic {
            AirPlayReceiverDiagnostic::RequestOutcome { stage, status } => {
                // This is the RTSP/HTTP transport status only. HAP can return HTTP 200
                // with a TLV error, so it does not prove that authentication succeeded.
                eprintln!("mirror_request stage={stage:?} transport_status={status}");
            }
            kind => eprintln!("mirror_diagnostic kind={kind:?}"),
        }
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        let _ = diagnostic;
    }
    fn on_initial_setup_observation(&mut self, observation: AirPlayInitialSetupObservation) {
        if self.diagnostic_events >= 32 {
            return;
        }
        self.diagnostic_events += 1;
        #[cfg(feature = "experimental_airplay_mirroring")]
        eprintln!(
            "mirror_initial_setup timing={:?} stream_count={} stream_types={:?} encryption_type_bits={:?} screen_mirroring={}",
            observation.timing_protocol,
            observation.stream_count,
            observation.stream_types.iter().take(8).collect::<Vec<_>>(),
            observation.encryption_type_bits,
            observation.is_screen_mirroring_session
        );
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        let _ = observation;
    }
    fn init_modes(&mut self) -> ChangeModes {
        ChangeModes::default()
    }
    async fn on_initial_setup(&mut self) -> RtspResult<()> {
        if self.handle.is_none() || self.car.is_closed() {
            Err(RtspError::Closed)
        } else {
            Ok(())
        }
    }
    async fn on_info(&mut self, info: &mut InfoMessageResponse) {
        use catplay_carplay::msg::{AudioFormatStruct, AudioLatency, AudioType, StreamType};
        let formats = super::mirror_input_audio_formats(self.car_pcm_formats);
        info.audio_formats.clear();
        info.audio_latencies.clear();
        if !formats.is_empty() {
            for stream_type in [StreamType::GeneralAudio, StreamType::MainAudio, StreamType::MainHighAudio] {
                info.audio_formats.push(AudioFormatStruct {
                    stream_type,
                    audio_type: Some(AudioType::Media),
                    audio_input_formats: None,
                    audio_output_formats: Some(formats),
                });
                info.audio_latencies.push(AudioLatency {
                    stream_type: Some(stream_type),
                    audio_type: Some(AudioType::Media),
                    input_latency_micros: None,
                    output_latency_micros: Some(32000),
                    ..Default::default()
                });
            }
        }
        info.displays = vec![Display {
            width_pixels: self.width,
            height_pixels: self.height,
            max_fps: Some(30),
            uuid: "catplay-mirror-car-display".into(),
            ..Default::default()
        }];
    }
    async fn on_command_raw(&mut self, _command: &Command) -> RtspResult<RtspResponse> {
        Err(RtspError::NotSupported)
    }
    async fn open_screen(&mut self, _latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        if self.handle.is_none()
            || self.car.is_closed()
            || self
                .screen_status
                .as_ref()
                .is_some_and(|status| !status.is_closed())
        {
            return Err(RtspError::NotSupported);
        }
        let limits = MirrorLimits::new(
            self.width,
            self.height,
            self.options.h264_profile_idc,
            self.options.h264_level_idc,
            AIRPLAY_MIRRORING_MAX_SCREEN_PAYLOAD,
        )?;
        let (sink, receiver) = MirrorBridge::new(limits);
        self.screen_status = Some(receiver.stream_status());
        self.car
            .unbounded_send(TxAdapterOp::MirrorScreen {
                id: self.id.clone(),
                receiver,
            })
            .map_err(|_| RtspError::Closed)?;
        Ok(Box::new(sink))
    }
    async fn open_audio(
        &mut self,
        latency: Duration,
        stream_type: catplay_carplay::msg::StreamType,
        audio_type: catplay_carplay::msg::AudioType,
        audio_format: catplay_carplay::msg::AudioFormat,
        pcm_format: catplay_carplay::audio::AudioStreamBasicDescription,
        duplex: bool,
    ) -> RtspResult<catplay_carplay::audio::AudioPlayerBox<i16>> {
        use catplay_carplay::msg::StreamType;
        if self.handle.is_none()
            || !self.id.is_active()
            || self.car.is_closed()
            || duplex
            || !matches!(
                audio_type,
                catplay_carplay::msg::AudioType::Media | catplay_carplay::msg::AudioType::Default
            )
            || !matches!(
                stream_type,
                StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio
            )
            || audio_format.bits().count_ones() != 1
            || !super::mirror_input_audio_formats(self.car_pcm_formats).contains(audio_format)
            || !super::mirror_pcm_supported(self.car_pcm_formats, pcm_format)
            || self
                .audio_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.is_active())
        {
            return Err(RtspError::NotSupported);
        }
        let attempt = super::MirrorAudioAttempt::new();
        self.audio_attempt = Some(attempt.clone());
        let (tx, rx) = catplay_util::oneshot::channel();
        let sent = self.car.unbounded_send(TxAdapterOp::MirrorSetupAudio {
            id: self.id.clone(),
            attempt: attempt.clone(),
            latency,
            pcm_format,
            response: tx,
        });
        let result = if sent.is_err() {
            Err(RtspError::Closed)
        } else {
            match tokio::time::timeout(Duration::from_secs(5), rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(RtspError::Closed),
                Err(_) => Err(RtspError::Timeout),
            }
        };
        match result {
            Ok(player) if self.id.is_active() && attempt.is_active() => Ok(Box::new(super::MirrorAudioPlayer::new(
                player,
                self.id.clone(),
                attempt,
                self.car.clone(),
            ))),
            result => {
                attempt.retire();
                let error = match result {
                    Err(error) => error,
                    Ok(player) => {
                        // Retire the car guard before another actor can release
                        // ownership and drain its teardown queue.
                        drop(player);
                        RtspError::Closed
                    }
                };
                let _ = self.car.unbounded_send(TxAdapterOp::MirrorAudioEnded {
                    id: self.id.clone(),
                    attempt,
                });
                Err(error)
            }
        }
    }
    // Default open_microphone/open_iap2 reject or return None.
}
impl EventSleeper for MirrorRxSession {}
impl EventReconciler for MirrorRxSession {
    type Error = RtspError;
}
impl AsyncShutdown for MirrorRxSession {
    async fn shutdown(&mut self) {
        self.finish();
    }
}
impl Drop for MirrorRxSession {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handle(profile: catplay_carplay::carplay_rx::AirPlayReceiverProfile) -> AirPlayReceiverHandleRef {
        Arc::new(catplay_carplay::carplay_rx::sink::AirPlayReceiverHandleImpl::new_with_profile(
            None,
            catplay_carplay::clock::TimingClient::new().1,
            mpsc::unbounded().0,
            "lo",
            true,
            mpsc::channel(1).0,
            profile,
        ))
    }
    fn session(tx: mpsc::UnboundedSender<TxAdapterOp>) -> MirrorRxSession {
        MirrorRxSession::new(
            tx,
            MirroringOptions {
                h264_profile_idc: 66,
                h264_level_idc: 31,
            },
            800,
            480,
        )
    }
    #[tokio::test]
    async fn shutdown_is_tagged_once_and_late_drop_does_not_disconnect_successor() {
        use catplay_carplay::carplay_rx::AirPlayReceiverProfile;
        let (tx, mut rx) = mpsc::unbounded();
        let mut old = session(tx.clone());
        old.init(handle(AirPlayReceiverProfile::AppleTVH264VideoOnly))
            .unwrap();
        let id = match rx.try_next().unwrap().unwrap() {
            TxAdapterOp::MirrorConnected { id, .. } => id,
            _ => panic!("expected mirror admission"),
        };
        old.shutdown().await;
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::MirrorDisconnected { id: released })) if released.matches(&id)));
        let mut next = session(tx);
        next.init(handle(AirPlayReceiverProfile::AppleTVH264VideoOnly))
            .unwrap();
        let successor = match rx.try_next().unwrap().unwrap() {
            TxAdapterOp::MirrorConnected { id, .. } => id,
            _ => panic!("expected successor admission"),
        };
        assert!(!id.matches(&successor));
        old.shutdown().await;
        drop(old);
        assert!(rx.try_next().is_err());
        next.shutdown().await;
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::MirrorDisconnected { id: released })) if released.matches(&successor)));
    }
    #[tokio::test]
    async fn refuses_carplay_identity_controls_and_uninitialized_screen() {
        use catplay_carplay::{carplay_rx::AirPlayReceiverProfile, msg::CommandForceKeyFrame};
        let (tx, _rx) = mpsc::unbounded();
        let mut sink = session(tx);
        assert!(sink.init(handle(AirPlayReceiverProfile::CarPlay)).is_err());
        assert!(sink.open_screen(Duration::ZERO).await.is_err());
        assert!(
            sink.on_command_raw(&Command::ForceKeyFrame(CommandForceKeyFrame {}))
                .await
                .is_err()
        );
        assert!(sink.open_iap2().await.is_none());
    }
    #[test]
    fn session_tokens_do_not_match_foreign_or_successor_owners() {
        let old = MirrorSessionId::new();
        assert!(old.matches(&old.clone()));
        assert!(!old.matches(&MirrorSessionId::new()));
    }
    #[test]
    fn diagnostics_are_capped_per_connection_but_eof_is_reserved_once() {
        let (tx, _rx) = mpsc::unbounded();
        let mut sink = session(tx);
        for _ in 0..40 {
            sink.on_receiver_diagnostic(AirPlayReceiverDiagnostic::RequestOutcome {
                stage: catplay_carplay::carplay_rx::AirPlayReceiverRequestStage::Unknown,
                status: 404,
            });
        }
        assert_eq!(sink.diagnostic_events, 32);
        sink.on_receiver_diagnostic(AirPlayReceiverDiagnostic::ConnectionEnded(
            catplay_carplay::carplay_rx::AirPlayReceiverTransportClass::Transport,
        ));
        sink.on_receiver_diagnostic(AirPlayReceiverDiagnostic::ConnectionEnded(
            catplay_carplay::carplay_rx::AirPlayReceiverTransportClass::Transport,
        ));
        assert!(sink.connection_ended_reported);
        assert_eq!(sink.diagnostic_events, 32);
    }
    #[test]
    fn uninitialized_sink_drop_does_not_disconnect_a_session() {
        let (tx, mut rx) = mpsc::unbounded();
        drop(MirrorRxSession::new(
            tx,
            MirroringOptions {
                h264_profile_idc: 66,
                h264_level_idc: 31,
            },
            800,
            480,
        ));
        assert!(matches!(rx.try_next(), Ok(None)));
    }
}
