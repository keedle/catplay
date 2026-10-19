use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use catplay_util::{event_select, notify::Notify};
use log::debug;

use crate::{
    screen::tx::{
        screen_tx_session::ScreenTransmitOp,
        screen_tx_sink::{ScreenTransmitError, ScreenTransmitSink},
    },
    video::{AvccConfigExtended, EncodedVideoFrame},
};
pub struct ScreenTransmitProxy {
    stream_latency: Duration,

    queue: Arc<Mutex<VecDeque<ScreenTransmitOp>>>,
    closed: Arc<Mutex<bool>>,
    notify_frame_added: Notify,
    notify_frame_consumed: Notify,
    max_queued_frames: usize,
    last_config: Option<AvccConfigExtended>,

    needs_keyframe: bool,
}

impl ScreenTransmitProxy {
    pub fn new(
        stream_latency: Duration,
        queue: Arc<Mutex<VecDeque<ScreenTransmitOp>>>,
        closed: Arc<Mutex<bool>>,
        notify_frame_added: Notify,
        notify_frame_consumed: Notify,
        max_queue: usize,
    ) -> Self {
        Self {
            stream_latency,
            queue,
            closed,
            notify_frame_added,
            notify_frame_consumed,
            max_queued_frames: max_queue,
            last_config: None,
            needs_keyframe: true,
        }
    }

    /// Drop queued media and require an IDR before the next frame is sent.
    /// Used by the experimental mirror path after a gap or stream change.
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn reset_for_keyframe(&mut self) {
        if *self.closed.lock().unwrap() {
            return;
        }

        let mut ops = self.queue.lock().unwrap();
        ops.retain(ScreenTransmitOp::is_shutdown);
        self.last_config = None;
        self.needs_keyframe = true;
        drop(ops);

        self.notify_frame_added.notify();
        self.notify_frame_consumed.notify();
    }
}

impl ScreenTransmitSink for ScreenTransmitProxy {
    fn stream_latency(&self) -> Duration {
        self.stream_latency
    }

    fn queue_len(&self) -> usize {
        let ops = self.queue.lock().unwrap();
        ops.iter().filter(|op| op.is_frame()).count()
    }

    fn queue_max(&self) -> usize {
        self.max_queued_frames
    }

    fn is_full(&self) -> bool {
        self.queue_len() > self.queue_max()
    }

    fn is_closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }

    fn push_avcc_config(&mut self, config: AvccConfigExtended) -> Result<(), ScreenTransmitError> {
        let closed = self.closed.lock().unwrap();
        if *closed {
            return Err(ScreenTransmitError::Closed);
        }

        if let Some(last_config) = self.last_config.as_ref()
            && last_config == &config
        {
            return Ok(());
        }

        let mut ops = self.queue.lock().unwrap();
        ops.push_back(ScreenTransmitOp::Configure(config.clone()));
        self.last_config.replace(config);
        self.notify_frame_added.notify();

        Ok(())
    }

    fn needs_keyframe(&self) -> bool {
        self.needs_keyframe
    }

    async fn push_frame(&mut self, frame: EncodedVideoFrame) -> Result<(), (ScreenTransmitError, EncodedVideoFrame)> {
        if *self.closed.lock().unwrap() {
            return Err((ScreenTransmitError::Closed, frame));
        }

        if let Some(config) = frame.config.clone()
            && let Err(err) = self.push_avcc_config(config)
        {
            return Err((err, frame));
        }

        if self.last_config.is_none() {
            return Err((ScreenTransmitError::NotConfigured, frame));
        }

        if self.needs_keyframe && !frame.is_known_keyframe() {
            return Err((ScreenTransmitError::NeedsKeyframe, frame));
        }

        loop {
            let overflow = {
                let ops = self.queue.lock().unwrap();
                let queued = ops.iter().filter(|op| op.is_frame()).count();
                queued > self.max_queued_frames
            };

            if *self.closed.lock().unwrap() {
                return Err((ScreenTransmitError::Closed, frame));
            }

            if overflow {
                // return Err(ScreenTransmitError::QueueOverflow);
                event_select!(self.notify_frame_consumed);
            } else {
                let mut ops = self.queue.lock().unwrap();
                ops.push_back(ScreenTransmitOp::Frame(frame));
                self.notify_frame_added.notify();
                self.needs_keyframe = false;
                break;
            }
        }

        Ok(())
    }
}

impl Drop for ScreenTransmitProxy {
    fn drop(&mut self) {
        debug!("Screen transmitter was dropped!");
        // self.queue.lock().unwrap().push_back(ScreenTransmitOp::TransmitterDropped);
        self.notify_frame_added.notify();
    }
}

#[cfg(all(test, feature = "experimental_airplay_mirroring"))]
mod tests {
    use super::*;
    use crate::video::{AvccConfig, Pts};
    use std::time::Instant;

    fn config(latency: Duration) -> AvccConfigExtended {
        AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: vec![0, 0, 0, 1, 0x67, 66, 0, 30, 0, 0, 0, 1, 0x68, 0],
            },
            video_latency: latency,
            width: 800,
            height: 480,
            respect_timestamps: true,
        }
    }

    fn frame(is_keyframe: bool, config: Option<AvccConfigExtended>) -> EncodedVideoFrame {
        EncodedVideoFrame {
            pts: Pts(Instant::now()),
            width: 800,
            height: 480,
            data: bytes::BytesMut::from(&b"frame"[..]),
            config,
            nal_offsets: None,
            is_keyframe: Some(is_keyframe),
            header_buf: bytes::BytesMut::new(),
            chacha_tag_buf: bytes::BytesMut::new(),
        }
    }

    fn proxy() -> (ScreenTransmitProxy, Arc<Mutex<VecDeque<ScreenTransmitOp>>>) {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let proxy = ScreenTransmitProxy::new(
            Duration::ZERO,
            queue.clone(),
            Arc::new(Mutex::new(false)),
            Notify::new(),
            Notify::new(),
            4,
        );
        (proxy, queue)
    }

    #[tokio::test]
    async fn reset_drops_queued_media_and_requires_configured_idr() {
        let mut proxy = proxy().0;
        let old_config = config(Duration::ZERO);
        proxy
            .push_frame(frame(true, Some(old_config.clone())))
            .await
            .unwrap();
        proxy.push_frame(frame(false, None)).await.unwrap();
        assert_eq!(proxy.queue_len(), 2);
        assert!(!proxy.needs_keyframe());

        proxy.reset_for_keyframe();
        assert_eq!(proxy.queue_len(), 0);
        assert!(proxy.needs_keyframe());
        assert!(proxy.last_config.is_none());
        assert!(matches!(
            proxy.push_frame(frame(false, None)).await,
            Err((ScreenTransmitError::NotConfigured, _))
        ));

        let new_config = config(Duration::from_millis(20));
        proxy
            .push_frame(frame(true, Some(new_config)))
            .await
            .unwrap();
        let queue = proxy.queue.lock().unwrap();
        assert!(matches!(queue.front(), Some(ScreenTransmitOp::Configure(_))));
        assert!(matches!(queue.get(1), Some(ScreenTransmitOp::Frame(_))));
        assert_eq!(queue.len(), 2);
        assert!(!proxy.needs_keyframe());
    }

    #[test]
    fn reset_preserves_transmitter_shutdown_markers() {
        let (mut proxy, queue) = proxy();
        proxy.push_avcc_config(config(Duration::ZERO)).unwrap();
        queue
            .lock()
            .unwrap()
            .push_back(ScreenTransmitOp::TransmitterDropped);

        proxy.reset_for_keyframe();

        let ops = queue.lock().unwrap();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops.front(), Some(ScreenTransmitOp::TransmitterDropped)));
        assert!(proxy.needs_keyframe());
        assert!(proxy.last_config.is_none());
    }
}
