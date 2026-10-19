use std::{
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use catplay_carplay::{
    audio::{AudioPlayer, AudioRecorder, AudioSinkBox, AudioSourceBox, RingBuffer, RingConsumer, RingProducer},
    carplay_tx::TeardownGuard,
    msg::StreamType,
    rtsp_frame::RtspResult,
};
use log::debug;
use tokio::{sync::watch, task::JoinHandle, time::sleep};

#[inline]
pub fn duration_to_frames_u32_saturating(d: Duration, freq: usize) -> u32 {
    let ns = d.as_nanos(); // u128
    let freq = freq as u128;

    let samples = ns.saturating_mul(freq) / 1_000_000_000u128;

    samples.min(u32::MAX as u128) as u32
}

fn duration_to_frames_usize_saturating(duration: Duration, frequency: usize) -> usize {
    let frames = duration.as_nanos().saturating_mul(frequency as u128) / 1_000_000_000u128;
    frames.min(usize::MAX as u128) as usize
}

/// Represents along with [PcmProxyRecorder] a loopback pair of AudioPlayer and AudioRecorder that exchange PCM samples using
/// ALSA-like high accuracy timing thread and [RingBuffer].
pub struct PcmProxyPlayer {
    ring: Option<(RingProducer<i16>, RingConsumer<i16>)>,
    source: Option<AudioSourceBox<i16>>,
    task: Option<JoinHandle<()>>,
    pub guard: Arc<Mutex<Option<TeardownGuard<StreamType>>>>,

    sample_rate: u32,
    channels_per_frame: usize,
    interval: Duration,
    receiver: Option<mpsc::Receiver<AudioSinkBox<i16>>>,
    limit: usize,
    control: watch::Sender<PlaybackControl>,
    epoch: u64,
    io_gate: Arc<Mutex<()>>,
}

#[derive(Clone, Copy, Default)]
struct PlaybackControl {
    running: bool,
    epoch: u64,
}

impl PcmProxyPlayer {
    pub fn new(
        ring: (RingProducer<i16>, RingConsumer<i16>),
        sample_rate: u32,
        interval: Duration,
        channels_per_frame: usize,
        receiver: mpsc::Receiver<AudioSinkBox<i16>>,
        limit: usize,
    ) -> Self {
        let (control, _control_rx) = watch::channel(PlaybackControl::default());
        Self {
            ring: Some(ring),
            source: None,
            task: None,
            guard: Default::default(),

            sample_rate,
            channels_per_frame,
            interval,
            receiver: Some(receiver),
            limit,
            control,
            epoch: 0,
            io_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn pair(sample_rate: u32, channels_per_frame: usize) -> (PcmProxyPlayer, PcmProxyRecorder) {
        let _latency = Duration::from_millis(32);

        const INTERVAL: Duration = Duration::from_millis(10);
        const RING_INTERVAL_MULTIPLIER: usize = 50; // 10ms interval, 500ms ring

        let channel = mpsc::channel();

        let ring_size_samples =
            duration_to_frames_u32_saturating(INTERVAL, sample_rate as _) as usize * channels_per_frame * RING_INTERVAL_MULTIPLIER;

        let ring = RingBuffer::spsc(ring_size_samples);
        let player = PcmProxyPlayer::new(ring, sample_rate, INTERVAL, channels_per_frame, channel.1, ring_size_samples);
        let recorder = PcmProxyRecorder::new(channel.0);
        (player, recorder)
    }
}

impl AudioPlayer for PcmProxyPlayer {
    type Sample = i16;

    fn init(&mut self, source: AudioSourceBox<Self::Sample>) -> RtspResult<()> {
        let _ring = self.ring.take().unwrap();
        self.source.replace(source);

        Ok(())
    }

    fn start(&mut self) {
        debug!("PcmProxyPlayer: start");
        self.set_running(true);
        if self.task.is_some() {
            return;
        }

        let Some(source) = self.source.take() else {
            debug!("PcmProxyPlayer: start without source");
            return;
        };
        let Some(receiver) = self.receiver.take() else {
            debug!("PcmProxyPlayer: start without recorder receiver");
            return;
        };
        let sample_rate = self.sample_rate;
        let channels_per_frame = self.channels_per_frame;
        let interval = self.interval;
        let limit = self.limit;
        let mut control_rx = self.control.subscribe();
        let io_gate = self.io_gate.clone();

        // Keep one worker and both audio endpoints for this player's lifetime.
        // stop/start only changes control state; only Drop aborts the worker.
        self.task = Some(tokio::spawn(async move {
            run_proxy(
                source,
                receiver,
                &mut control_rx,
                io_gate,
                sample_rate,
                channels_per_frame,
                interval,
                limit,
            )
            .await;
        }));
    }

    fn stop(&mut self, _drain: bool) {
        debug!("PcmProxyPlayer: stop");
        let gate = self.io_gate.clone();
        let _io = gate.lock().unwrap();
        self.set_running(false);
    }
}

impl PcmProxyPlayer {
    fn set_running(&mut self, running: bool) {
        let current = *self.control.borrow();
        if current.running == running {
            return;
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("PCM proxy epoch exhausted");
        self.control.send_replace(PlaybackControl {
            running,
            epoch: self.epoch,
        });
    }
}

async fn run_proxy(
    mut player: AudioSourceBox<i16>,
    receiver: mpsc::Receiver<AudioSinkBox<i16>>,
    control_rx: &mut watch::Receiver<PlaybackControl>,
    io_gate: Arc<Mutex<()>>,
    sample_rate: u32,
    channels_per_frame: usize,
    interval: Duration,
    limit: usize,
) {
    // We have buffered audio, but do not stream too far ahead or the receiver
    // may start dropping data. The cap below still bounds each write.
    let read_ahead = Duration::from_millis(150);
    let mut recorder: Option<AudioSinkBox<i16>> = None;
    let mut observed_epoch = control_rx.borrow().epoch;
    let mut pace_start = Instant::now();
    let mut pace_consumed = player.stat_raw().consumed;
    let mut buffer = Vec::with_capacity(limit);

    loop {
        let control = *control_rx.borrow_and_update();
        if !control.running {
            if control_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        if control.epoch != observed_epoch {
            observed_epoch = control.epoch;
            pace_start = Instant::now();
            pace_consumed = player.stat_raw().consumed;
        }

        if recorder.is_none() {
            match receiver.try_recv() {
                Ok(sink) => {
                    debug!("Received recorder!");
                    recorder = Some(sink);
                    pace_start = Instant::now();
                    pace_consumed = player.stat_raw().consumed;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Keep the source and worker alive through temporary pause
                    // and allow the same endpoint to be resumed if it remains.
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if let Some(sink) = recorder.as_mut() {
            let _io = io_gate.lock().unwrap();
            // stop() takes the same gate before publishing pause, so it returns
            // only after any current read/write has completed. This second
            // check prevents a tick queued before stop from running afterward.
            let latest = *control_rx.borrow();
            if latest.running && latest.epoch == observed_epoch {
                let stat = player.stat_raw();
                let elapsed = Instant::now().saturating_duration_since(pace_start) + read_ahead;
                let ahead = duration_to_frames_usize_saturating(elapsed, sample_rate as usize).saturating_mul(channels_per_frame);
                let allowed = pace_consumed.saturating_add(ahead).min(stat.written);
                let readable = allowed.saturating_sub(stat.consumed).min(limit);
                if readable > 0 {
                    buffer.clear();
                    buffer.resize(readable, 0);
                    player.read(&mut buffer);
                    sink.write(&buffer);
                }
            }
        }

        tokio::select! {
            _ = sleep(interval) => {},
            changed = control_rx.changed() => {
                if changed.is_err() { return; }
            }
        }
    }
}

impl Drop for PcmProxyPlayer {
    fn drop(&mut self) {
        debug!("PcmProxyPlayer: drop");
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub struct PcmProxyRecorder {
    sender: mpsc::Sender<AudioSinkBox<i16>>,
    pub guard: Arc<Mutex<Option<TeardownGuard<StreamType>>>>,
}

impl PcmProxyRecorder {
    pub fn new(sender: mpsc::Sender<AudioSinkBox<i16>>) -> Self {
        Self {
            guard: Default::default(),
            sender,
        }
    }
}

impl AudioRecorder for PcmProxyRecorder {
    type Sample = i16;

    fn init(&mut self, source: AudioSinkBox<Self::Sample>) -> RtspResult<()> {
        if let Err(err) = self.sender.send(source) {
            debug!("PcmProxyRecorder failed to send source: {err:?}");
        }

        Ok(())
    }

    fn start(&mut self) {
        debug!("PcmProxyRecorder: start (ignored)");
    }

    fn stop(&mut self, drain: bool) {
        debug!("PcmProxyRecorder: stop {drain} (ignored)");
    }
}

impl Drop for PcmProxyRecorder {
    fn drop(&mut self) {
        debug!("PcmProxyRecorder: drop");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_carplay::audio::{AudioSink, AudioSource, RingStatsAudio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::time::{sleep, timeout};

    struct RingSource {
        consumer: RingConsumer<i16>,
        sample_rate: usize,
        start: Instant,
        reads: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    impl AudioSource for RingSource {
        type Sample = i16;

        fn stat(&mut self) -> RingStatsAudio {
            self.consumer
                .stat()
                .as_audio_stats(self.sample_rate, 1, self.start)
        }

        fn stat_raw(&mut self) -> catplay_carplay::audio::RingStats {
            self.consumer.stat()
        }

        fn read(&mut self, slice: &mut [i16]) -> bool {
            self.reads.fetch_add(slice.len(), Ordering::SeqCst);
            if self.consumer.read(slice).is_err() {
                slice.fill(0);
            }
            true
        }
    }

    impl Drop for RingSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    struct LargeCounterSource {
        counts: Arc<Mutex<(usize, usize)>>,
        sample_rate: usize,
        start: Instant,
        reads: Arc<AtomicUsize>,
    }

    impl AudioSource for LargeCounterSource {
        type Sample = i16;

        fn stat(&mut self) -> RingStatsAudio {
            self.stat_raw()
                .as_audio_stats(self.sample_rate, 1, self.start)
        }

        fn stat_raw(&mut self) -> catplay_carplay::audio::RingStats {
            let (written, consumed) = *self.counts.lock().unwrap();
            catplay_carplay::audio::RingStats {
                capacity: written,
                written,
                consumed,
                readable: written.saturating_sub(consumed),
                overflow: 0,
                underflow: 0,
            }
        }

        fn read(&mut self, slice: &mut [i16]) -> bool {
            self.reads.fetch_add(slice.len(), Ordering::SeqCst);
            self.counts.lock().unwrap().1 += slice.len();
            slice.fill(5);
            true
        }
    }

    struct CaptureSink {
        samples: Arc<AtomicUsize>,
        writes: Arc<AtomicUsize>,
    }

    impl AudioSink for CaptureSink {
        type Sample = i16;

        fn writable(&mut self) -> usize {
            usize::MAX
        }

        fn write(&mut self, samples: &[i16]) {
            self.samples.fetch_add(samples.len(), Ordering::SeqCst);
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn setup() -> (
        PcmProxyPlayer,
        PcmProxyRecorder,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
    ) {
        let ring_size = 4096;
        let dummy_ring = RingBuffer::spsc(128);
        let (mut source_producer, source_consumer) = RingBuffer::spsc(ring_size);
        source_producer.write(&vec![7i16; ring_size]);

        let reads = Arc::new(AtomicUsize::new(0));
        let samples = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let (sink_tx, sink_rx) = mpsc::channel();
        let mut recorder = PcmProxyRecorder::new(sink_tx);
        recorder
            .init(Box::new(CaptureSink {
                samples: samples.clone(),
                writes: writes.clone(),
            }))
            .unwrap();

        let mut proxy = PcmProxyPlayer::new(dummy_ring, 1000, Duration::from_millis(10), 1, sink_rx, 32);
        proxy
            .init(Box::new(RingSource {
                consumer: source_consumer,
                sample_rate: 1000,
                start: Instant::now(),
                reads: reads.clone(),
                dropped: dropped.clone(),
            }))
            .unwrap();
        (proxy, recorder, reads, samples, writes, dropped)
    }

    async fn wait_for_samples(counter: &AtomicUsize, baseline: usize) {
        timeout(Duration::from_millis(300), async {
            while counter.load(Ordering::SeqCst) <= baseline {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("audio worker did not resume");
    }

    #[tokio::test]
    async fn one_source_survives_pause_resume_and_rapid_restart_until_drop() {
        let (mut proxy, _recorder, reads, samples, writes, dropped) = setup();
        proxy.start();
        wait_for_samples(&reads, 0).await;
        assert!(writes.load(Ordering::SeqCst) > 0);

        proxy.stop(false);
        let paused_reads = reads.load(Ordering::SeqCst);
        let paused_writes = writes.load(Ordering::SeqCst);
        sleep(Duration::from_millis(35)).await;
        assert_eq!(reads.load(Ordering::SeqCst), paused_reads);
        assert_eq!(writes.load(Ordering::SeqCst), paused_writes);

        proxy.start();
        wait_for_samples(&reads, paused_reads).await;
        let resumed_reads = reads.load(Ordering::SeqCst);
        proxy.stop(false);
        proxy.start();
        wait_for_samples(&reads, resumed_reads).await;
        assert!(samples.load(Ordering::SeqCst) > 0);
        assert!(!dropped.load(Ordering::SeqCst));

        drop(proxy);
        timeout(Duration::from_millis(300), async {
            while !dropped.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("source remained owned after player drop");
    }

    #[tokio::test]
    async fn long_pause_does_not_catch_up_using_elapsed_paused_time() {
        let (mut proxy, _recorder, reads, _, _, _) = setup();
        proxy.start();
        wait_for_samples(&reads, 0).await;
        proxy.stop(false);
        sleep(Duration::from_millis(220)).await;
        let before_resume = reads.load(Ordering::SeqCst);
        proxy.start();
        sleep(Duration::from_millis(25)).await;
        let resumed = reads.load(Ordering::SeqCst).saturating_sub(before_resume);
        // At 1 kHz, the 150 ms read-ahead plus 25 ms resumed time bounds this
        // burst independently of the 220 ms pause duration.
        assert!(resumed <= 200, "unexpected resume burst: {resumed} samples");
    }

    #[tokio::test]
    #[cfg(target_pointer_width = "64")]
    async fn playback_counters_above_u32_resume_without_wrapping() {
        let base = u32::MAX as usize + 10_000;
        let counts = Arc::new(Mutex::new((base + 2_000, base)));
        let reads = Arc::new(AtomicUsize::new(0));
        let (sink_tx, sink_rx) = mpsc::channel();
        let mut recorder = PcmProxyRecorder::new(sink_tx);
        recorder
            .init(Box::new(CaptureSink {
                samples: Arc::new(AtomicUsize::new(0)),
                writes: Arc::new(AtomicUsize::new(0)),
            }))
            .unwrap();
        let mut proxy = PcmProxyPlayer::new(RingBuffer::spsc(128), 1000, Duration::from_millis(10), 1, sink_rx, 32);
        proxy
            .init(Box::new(LargeCounterSource {
                counts,
                sample_rate: 1000,
                start: Instant::now(),
                reads: reads.clone(),
            }))
            .unwrap();
        proxy.start();
        wait_for_samples(&reads, 0).await;
        assert!(reads.load(Ordering::SeqCst) <= 32);
    }
}
