use async_trait::async_trait;
use catplay_util::AsyncShutdown;

use crate::{
    rtsp_frame::{RtspError, RtspResult},
    screen::rx::{ScreenReceiverSink, ScreenReceiverSinkBox},
    video::{AvccConfigExtended, EncodedVideoFrame},
};

/// Enforce the opt-in profile even when a sender disregards the advertisement.
/// This only checks codec/framing. Decoder capability admission belongs to the
/// consumer; there is no head-unit integration in this receiver experiment.
pub(super) struct PictureOnlyScreenSink {
    inner: ScreenReceiverSinkBox,
    configured: bool,
}

impl PictureOnlyScreenSink {
    pub fn new(inner: ScreenReceiverSinkBox) -> Self {
        Self { inner, configured: false }
    }

    fn validate_config(config: &AvccConfigExtended) -> RtspResult<()> {
        if config.hevc {
            return Err(RtspError::ProtocolViolation("picture-only mirroring does not support HEVC"));
        }
        if config.avcc.nal_size_len != 4 {
            return Err(RtspError::ProtocolViolation(
                "picture-only mirroring requires four-byte NAL lengths",
            ));
        }
        if config.width == 0 || config.height == 0 {
            return Err(RtspError::ProtocolViolation(
                "picture-only mirroring requires nonzero coded dimensions",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ScreenReceiverSink for PictureOnlyScreenSink {
    async fn init(&mut self) -> RtspResult<()> {
        self.inner.init().await
    }

    async fn set_avcc_config(&mut self, config: AvccConfigExtended) -> RtspResult<()> {
        self.configured = false;
        Self::validate_config(&config)?;
        self.inner.set_avcc_config(config).await?;
        self.configured = true;
        Ok(())
    }

    async fn process_frame(&mut self, frame: EncodedVideoFrame) -> RtspResult<()> {
        if !self.configured {
            return Err(RtspError::ProtocolViolation(
                "picture-only mirroring received frame without accepted H264 config",
            ));
        }
        if let Some(config) = &frame.config {
            Self::validate_config(config)?;
        }
        self.inner.process_frame(frame).await
    }
}

impl AsyncShutdown for PictureOnlyScreenSink {
    async fn shutdown(&mut self) {
        self.configured = false;
        self.inner.shutdown_pinned().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::AvccConfig;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    struct Sink(Arc<Mutex<usize>>);
    #[async_trait]
    impl ScreenReceiverSink for Sink {
        async fn init(&mut self) -> RtspResult<()> {
            Ok(())
        }
        async fn set_avcc_config(&mut self, _config: AvccConfigExtended) -> RtspResult<()> {
            *self.0.lock().unwrap() += 1;
            Ok(())
        }
        async fn process_frame(&mut self, _frame: EncodedVideoFrame) -> RtspResult<()> {
            Ok(())
        }
    }
    impl AsyncShutdown for Sink {}

    #[tokio::test]
    async fn config_gate_rejects_hevc_legacy_lengths_and_zero_size_without_forwarding() {
        let calls = Arc::new(Mutex::new(0));
        let mut sink = PictureOnlyScreenSink::new(Box::new(Sink(calls.clone())));
        let config = AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: vec![],
            },
            video_latency: Duration::ZERO,
            width: 800,
            height: 480,
            respect_timestamps: true,
        };
        sink.set_avcc_config(config.clone()).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
        for invalid in [
            AvccConfigExtended {
                hevc: true,
                ..config.clone()
            },
            AvccConfigExtended {
                width: 0,
                ..config.clone()
            },
            AvccConfigExtended {
                avcc: AvccConfig {
                    nal_size_len: 2,
                    sps_pps: vec![],
                },
                ..config.clone()
            },
        ] {
            assert!(sink.set_avcc_config(invalid).await.is_err());
            assert!(!sink.configured);
            assert_eq!(*calls.lock().unwrap(), 1);
        }
    }
}
