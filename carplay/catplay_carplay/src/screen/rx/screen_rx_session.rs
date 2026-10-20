use std::time::{Duration, Instant};

use async_trait::async_trait;
use catplay_tokio::{CItem, TcpSession, TcpSink};
use catplay_util::{AsyncShutdown, EventSleeper};
use log::{debug, trace, warn};

use crate::{
    cipher::AirPlayStreamEncryption,
    clock::MediaClockBox,
    rtsp_frame::{RtspError, RtspResult},
    screen::{
        ScreenFlag, ScreenFrameCodec, ScreenFrameCodecFlags, ScreenFrameHeader, ScreenOpCode,
        rx::screen_rx_sink::ScreenReceiverSinkBox,
    },
    video::AvccConfigExtended,
};

pub struct ScreenReceiverSession {
    cipher: AirPlayStreamEncryption,
    stream_connection_id: u64,

    clock: MediaClockBox,
    sink: ScreenReceiverSinkBox,
    video_latency: Duration,
    config: Option<AvccConfigExtended>,
    prev_frame: Option<Instant>,
    start: Instant,
    negative_ahead_frames: u32,
    late_frames: u32,
    display_delta_ms: i64,
    max_payload_size: usize,
    experimental_diagnostics: bool,
    video_config_header_reported: bool,
    codec_initialized_reported: bool,
}

impl ScreenReceiverSession {
    pub fn new(
        cipher: AirPlayStreamEncryption,
        stream_connection_id: u64,
        video_latency: Duration,
        sink: ScreenReceiverSinkBox,
        clock: MediaClockBox,
    ) -> Self {
        Self {
            cipher,
            stream_connection_id,
            config: None,
            video_latency,
            sink,
            clock,
            prev_frame: None,
            start: Instant::now(),
            negative_ahead_frames: 0,
            late_frames: 0,
            display_delta_ms: 0,
            max_payload_size: ScreenFrameCodec::MAX_PAYLOAD_SIZE,
            experimental_diagnostics: false,
            video_config_header_reported: false,
            codec_initialized_reported: false,
        }
    }

    pub fn with_experimental_diagnostics(mut self, enabled: bool) -> Self {
        self.experimental_diagnostics = enabled;
        self
    }

    /// Set a stricter receiver wire-body bound without changing legacy defaults.
    pub fn with_max_payload_size(mut self, limit: usize) -> RtspResult<Self> {
        if !(1..=ScreenFrameCodec::MAX_PAYLOAD_SIZE).contains(&limit) {
            return Err(RtspError::ProtocolViolation("invalid screen payload limit"));
        }
        self.max_payload_size = limit;
        Ok(self)
    }
}

#[async_trait]
impl TcpSession for ScreenReceiverSession {
    type Codec = ScreenFrameCodec;
    type Error = RtspError;

    fn init_codec(&mut self) -> RtspResult<ScreenFrameCodec> {
        let codec = ScreenFrameCodec::new_with_flags(
            self.cipher,
            self.stream_connection_id,
            true,
            ScreenFrameCodecFlags::crate_defaults(),
        )
        .with_max_payload_size(self.max_payload_size)?;
        if self.experimental_diagnostics && !self.codec_initialized_reported {
            eprintln!("screen_codec_initialized");
            self.codec_initialized_reported = true;
        }
        Ok(codec)
    }

    async fn on_msg(&mut self, _sink: &mut dyn TcpSink<Self>, msg: CItem<Self::Codec>) -> RtspResult<()> {
        if self.experimental_diagnostics {
            debug!("Received experimental screen frame");
        } else {
            debug!("Received screen frame: {:?} {:?}", msg.header, msg.flags());
        }

        match msg.header.opcode {
            ScreenOpCode::VideoFrame => {
                let Some(config) = self.config.as_ref() else {
                    return Err(RtspError::ProtocolViolation("received VideoFrame before VideoConfig"));
                };

                let _respect_timestamps = config.respect_timestamps;

                if !self.clock.is_synchronized() {
                    warn!("Received video frame before media clock was synchronized! Using pts = Instant::now()");
                }

                let frame = msg
                    .video_decode(config, self.clock.as_ref())
                    .map_err(|e| RtspError::ProtocolViolationString(format!("failed to scan NAL offsets in VideoFrame: {e:?}")))?
                    .ok_or(RtspError::Empty)?;

                let pts_decoded = frame.pts.decode();
                let display_ticks = frame.pts.0;
                let now = Instant::now();
                let video_latency_ms = self.video_latency.as_millis() as i64;

                let display_vs_now_ms = if display_ticks >= now {
                    display_ticks.saturating_duration_since(now).as_millis() as i64
                } else {
                    self.negative_ahead_frames = self.negative_ahead_frames.saturating_add(1);
                    -(now.saturating_duration_since(display_ticks).as_millis() as i64)
                };
                self.display_delta_ms = video_latency_ms - display_vs_now_ms;

                if self.display_delta_ms >= (2 * video_latency_ms) {
                    self.late_frames = self.late_frames.saturating_add(1);

                    warn!(
                        "Late frame ({} ms, {} total late frames, negativeAheadFrames={})",
                        self.display_delta_ms, self.late_frames, self.negative_ahead_frames
                    );
                }

                let recv_delta = match self.prev_frame {
                    None => {
                        let first_frame_took = now.saturating_duration_since(self.start);
                        warn!("Received first video frame after {first_frame_took:?}");
                        None
                    }
                    Some(v) => Some(now.saturating_duration_since(v)),
                };

                debug!("Decoded video frame pts={pts_decoded} recv_delta={recv_delta:?}");

                if frame.is_known_keyframe() {
                    warn!("Received keyframe");
                }

                self.prev_frame.replace(now);
                self.sink.process_frame(frame).await?;
            }

            ScreenOpCode::VideoConfig => {
                if self.experimental_diagnostics && !self.video_config_header_reported {
                    let (dimensions, view_size, crop_size) = video_config_header_geometry(&msg.header);
                    eprintln!(
                        "screen_video_config_header dimensions_width={} dimensions_height={} view_width={} view_height={} crop_width={} crop_height={}",
                        dimensions.0, dimensions.1, view_size.0, view_size.1, crop_size.0, crop_size.1
                    );
                    self.video_config_header_reported = true;
                }
                if msg.flags().contains(ScreenFlag::ClearScreen) {
                    warn!("Ignoring ScreenFlag::ClearScreen");
                    return Ok(());
                }
                let config = msg.config_decode(self.video_latency);
                let Some(config) = config else {
                    return Err(RtspError::ProtocolViolation("received unparsable VideoConfig frame"));
                };

                if self.experimental_diagnostics {
                    debug!("Received experimental VideoConfig");
                } else {
                    debug!("Received VideoConfig: {config:?}");
                }

                self.sink.set_avcc_config(config.clone()).await?;
                self.config.replace(config);
            }

            ScreenOpCode::KeepAlive | ScreenOpCode::KeepAliveWithBody => {
                trace!("Received screen keep alive: opcode {:?}", msg.header.opcode);

                if let Some(stats) = msg.sender_stats_decode() {
                    debug!("Received screen sender stats: {stats:?}");
                }
            }
            _ => {
                if self.experimental_diagnostics {
                    debug!("Received unknown experimental screen frame");
                } else {
                    debug!("Received unknown video frame: {:?}", msg.header);
                }
            }
        }
        Ok(())
    }

    async fn on_eof(&mut self, status: Option<RtspError>) {
        if self.experimental_diagnostics {
            warn!("Observed EOF on experimental screen socket");
        } else {
            warn!("Observed EOF on screen socket! {status:?}");
        }
    }
}

impl EventSleeper for ScreenReceiverSession {}

impl AsyncShutdown for ScreenReceiverSession {
    async fn shutdown(&mut self) {
        self.sink.shutdown_pinned().await;
    }
}

fn video_config_header_geometry(header: &ScreenFrameHeader) -> ((u32, u32), (u32, u32), (u32, u32)) {
    (
        header.params[1].as_f32_floor(),
        header.params[4].as_f32_floor(),
        header.params[6].as_f32_floor(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Value64;

    #[test]
    fn video_config_header_geometry_reads_only_fixed_numeric_pairs() {
        let mut header = ScreenFrameHeader::default();
        header.params[1] = Value64::from_f32_floor(800, 480);
        header.params[4] = Value64::from_f32_floor(640, 360);
        header.params[6] = Value64::from_f32_floor(320, 180);
        assert_eq!(video_config_header_geometry(&header), ((800, 480), (640, 360), (320, 180)));
    }
}
