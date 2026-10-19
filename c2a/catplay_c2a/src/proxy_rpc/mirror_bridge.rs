//! Bounded, picture-only AirPlay to CarPlay video bridge.
//!
//! The limits are supplied by the caller from explicit receiver/vehicle
//! settings. This module does not infer decoder support from observed streams.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::BytesMut;
use catplay_carplay::{
    rtsp_frame::{RtspError, RtspResult},
    screen::rx::ScreenReceiverSink,
    video::{AvccConfigExtended, EncodedVideoFrame},
};
use catplay_util::{AsyncShutdown, EventSleeper, EventToken};
use tokio::sync::Notify;

const PUBLISH_TIMEOUT: Duration = Duration::from_millis(500);
static NEXT_GENERATION: AtomicUsize = AtomicUsize::new(1);
const MAX_ACCESS_UNIT_NALS: usize = 4096;

/// Explicit receiver limits. Width and height must match the advertised
/// transport dimensions exactly; profile and level come from an explicit
/// policy. SPS parsing here checks only the profile and level header bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorLimits {
    pub width: u32,
    pub height: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub max_frame_bytes: usize,
}

impl MirrorLimits {
    pub fn new(width: u32, height: u32, profile_idc: u8, level_idc: u8, max_frame_bytes: usize) -> RtspResult<Self> {
        if width == 0 || height == 0 || max_frame_bytes == 0 {
            return Err(RtspError::ProtocolViolation("mirroring limits must be explicit and nonzero"));
        }
        Ok(Self {
            width,
            height,
            profile_idc,
            level_idc,
            max_frame_bytes,
        })
    }
}

/// Creates one issuer/generation-scoped producer and consumer pair.
pub struct MirrorBridge;

impl MirrorBridge {
    pub fn new(limits: MirrorLimits) -> (MirrorBridgeSink, MirrorBridgeReceiver) {
        let mut generation = NEXT_GENERATION.load(Ordering::Relaxed);
        loop {
            let next = generation
                .checked_add(1)
                .expect("mirror bridge generation space exhausted");
            match NEXT_GENERATION.compare_exchange_weak(generation, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => generation = actual,
            }
        }
        let state = Arc::new(Mutex::new(BridgeState {
            limits,
            generation,
            closed: false,
            config: None,
            pending: None,
            pending_revision: 0,
            revision: 0,
            needs_idr: true,
        }));
        let producer_notify = Arc::new(Notify::new());
        let consumer_notify = Arc::new(Notify::new());
        (
            MirrorBridgeSink {
                state: state.clone(),
                producer_notify: producer_notify.clone(),
                consumer_notify: consumer_notify.clone(),
            },
            MirrorBridgeReceiver {
                state,
                producer_notify,
                consumer_notify,
            },
        )
    }
}

struct BridgeState {
    limits: MirrorLimits,
    generation: usize,
    closed: bool,
    config: Option<AvccConfigExtended>,
    pending: Option<EncodedVideoFrame>,
    pending_revision: usize,
    revision: usize,
    needs_idr: bool,
}

/// AirPlay screen sink that validates each access unit and retains at most one
/// encoded frame for the connected-car event loop.
pub struct MirrorBridgeSink {
    state: Arc<Mutex<BridgeState>>,
    producer_notify: Arc<Notify>,
    consumer_notify: Arc<Notify>,
}

/// Consumer side owned by the connected-car event loop.
pub struct MirrorBridgeReceiver {
    state: Arc<Mutex<BridgeState>>,
    producer_notify: Arc<Notify>,
    consumer_notify: Arc<Notify>,
}

impl MirrorBridgeReceiver {
    pub fn generation(&self) -> usize {
        self.state.lock().unwrap().generation
    }

    /// Take the pending frame without waiting. The frame includes the accepted
    /// config so the existing transmitter configures before sending its IDR.
    pub fn take_frame(&mut self) -> Option<EncodedVideoFrame> {
        self.take_frame_tagged().map(|(_, frame)| frame)
    }

    /// Take a frame with the revision under which it was accepted. Call
    /// `is_revision_current` immediately before forwarding after any await.
    pub fn take_frame_tagged(&mut self) -> Option<(usize, EncodedVideoFrame)> {
        let mut state = self.state.lock().unwrap();
        let frame = state.pending.take();
        if let Some(frame) = frame {
            self.producer_notify.notify_one();
            return Some((state.pending_revision, frame));
        }
        None
    }

    pub fn revision(&self) -> usize {
        self.state.lock().unwrap().revision
    }

    pub fn is_revision_current(&self, revision: usize) -> bool {
        let state = self.state.lock().unwrap();
        !state.closed && state.revision == revision
    }

    /// Discard queued media and require a fresh IDR after downstream loss.
    pub fn invalidate_until_idr(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.pending.take();
        state.needs_idr = true;
        advance_revision(&mut state);
        self.producer_notify.notify_one();
        self.consumer_notify.notify_one();
    }

    /// Retire this generation. Old producer callbacks can no longer publish.
    pub fn close(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.pending.take();
        state.config.take();
        advance_revision(&mut state);
        self.producer_notify.notify_one();
        self.consumer_notify.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub fn has_pending_frame(&self) -> bool {
        self.state.lock().unwrap().pending.is_some()
    }
}

impl Drop for MirrorBridgeReceiver {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.pending.take();
        state.config.take();
        advance_revision(&mut state);
        self.producer_notify.notify_one();
        self.consumer_notify.notify_one();
    }
}

#[async_trait]
impl ScreenReceiverSink for MirrorBridgeSink {
    async fn init(&mut self) -> RtspResult<()> {
        self.ensure_open()
    }

    async fn set_avcc_config(&mut self, config: AvccConfigExtended) -> RtspResult<()> {
        let limits = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(closed_error());
            }
            // Clear old media before validating. A rejected renegotiation must
            // not leave the previous stream usable.
            state.pending.take();
            state.config.take();
            state.needs_idr = true;
            advance_revision(&mut state);
            self.producer_notify.notify_one();
            self.consumer_notify.notify_one();
            state.limits
        };
        if let Err(err) = validate_config(&config, limits) {
            return Err(err);
        }

        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(closed_error());
        }
        state.config = Some(config);
        state.needs_idr = true;
        advance_revision(&mut state);
        self.consumer_notify.notify_one();
        Ok(())
    }

    async fn process_frame(&mut self, mut frame: EncodedVideoFrame) -> RtspResult<()> {
        let (limits, needs_idr, accepted_revision) = {
            let state = self.state.lock().unwrap();
            if state.closed {
                return Err(closed_error());
            }
            let Some(config) = state.config.as_ref() else {
                return Err(RtspError::ProtocolViolation("mirror frame arrived before accepted H264 config"));
            };
            if frame
                .config
                .as_ref()
                .is_some_and(|frame_config| frame_config != config)
            {
                return Err(RtspError::ProtocolViolation("mirror frame config changed without reconfiguration"));
            }
            (state.limits, state.needs_idr, state.revision)
        };
        if frame.width != limits.width || frame.height != limits.height || frame.data.len() > limits.max_frame_bytes {
            return Err(RtspError::ProtocolViolation("mirror frame exceeds explicit target limits"));
        }
        let is_idr = validate_access_unit(&frame.data)?;
        if needs_idr && !is_idr {
            return Err(RtspError::ProtocolViolation("mirror stream must begin with a validated IDR"));
        }

        // Only IDRs carry config to the transmitter. Source-provided NAL offset
        // caches and crypto/header buffers are discarded; the car sender builds
        // its own framing and encryption metadata.
        let (accepted_config, accepted_config_bytes) = {
            let state = self.state.lock().unwrap();
            let config = if is_idr { state.config.clone() } else { None };
            let bytes = state
                .config
                .as_ref()
                .map(|value| value.avcc.sps_pps.capacity())
                .unwrap_or(0);
            (config, bytes)
        };
        frame.config = accepted_config;
        frame.nal_offsets.take();
        frame.header_buf = BytesMut::new();
        frame.chacha_tag_buf = BytesMut::new();
        let mut normalized_data = BytesMut::with_capacity(frame.data.len());
        normalized_data.extend_from_slice(&frame.data);
        frame.data = normalized_data;
        let retained_bytes = frame_retained_bytes(&frame).and_then(|bytes| bytes.checked_add(accepted_config_bytes));
        if retained_bytes.is_none_or(|bytes| bytes > limits.max_frame_bytes) {
            return Err(RtspError::ProtocolViolation("mirror frame exceeds explicit byte limits"));
        }

        frame.is_keyframe = Some(is_idr);
        loop {
            // Register before checking the slot to avoid missing a consumer wake.
            let notified = self.producer_notify.notified();
            {
                let mut state = self.state.lock().unwrap();
                if state.closed {
                    return Err(closed_error());
                }
                if state.revision != accepted_revision {
                    return Err(RtspError::ProtocolViolation("stale mirror generation revision"));
                }
                if is_idr && state.config.as_ref() != frame.config.as_ref() {
                    return Err(RtspError::ProtocolViolation("stale mirror generation config"));
                }
                if state.needs_idr && !is_idr {
                    return Err(RtspError::ProtocolViolation(
                        "mirror bridge was invalidated and requires a fresh IDR",
                    ));
                }
                if state.pending.is_none() {
                    state.pending = Some(frame);
                    state.pending_revision = state.revision;
                    state.needs_idr = false;
                    self.consumer_notify.notify_one();
                    return Ok(());
                }
            }
            if tokio::time::timeout(PUBLISH_TIMEOUT, notified)
                .await
                .is_err()
            {
                return Err(RtspError::ProtocolViolation("mirror consumer did not drain the bounded frame slot"));
            }
        }
    }
}

impl MirrorBridgeSink {
    fn ensure_open(&self) -> RtspResult<()> {
        if self.state.lock().unwrap().closed {
            Err(closed_error())
        } else {
            Ok(())
        }
    }
}

impl AsyncShutdown for MirrorBridgeSink {
    async fn shutdown(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.pending.take();
        state.config.take();
        advance_revision(&mut state);
        self.producer_notify.notify_one();
        self.consumer_notify.notify_one();
    }
}

impl Drop for MirrorBridgeSink {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.pending.take();
        state.config.take();
        advance_revision(&mut state);
        self.producer_notify.notify_one();
        self.consumer_notify.notify_one();
    }
}

impl EventSleeper for MirrorBridgeReceiver {
    async fn sleep(&mut self) -> Option<EventToken> {
        loop {
            let notified = self.consumer_notify.notified();
            {
                let state = self.state.lock().unwrap();
                if state.closed || state.pending.is_some() {
                    return Some(EventToken(state.revision as u64));
                }
            }
            notified.await;
        }
    }
}

fn closed_error() -> RtspError {
    RtspError::ProtocolViolation("mirror bridge generation is closed")
}

fn validate_config(config: &AvccConfigExtended, limits: MirrorLimits) -> RtspResult<()> {
    if config.hevc {
        return Err(RtspError::ProtocolViolation("mirror bridge accepts H264 only"));
    }
    if config.avcc.nal_size_len != 4 {
        return Err(RtspError::ProtocolViolation("mirror bridge requires four-byte AVCC lengths"));
    }
    if config.width != limits.width || config.height != limits.height {
        return Err(RtspError::ProtocolViolation(
            "mirror dimensions do not match explicit target limits",
        ));
    }
    if config.avcc.sps_pps.capacity() > limits.max_frame_bytes {
        return Err(RtspError::ProtocolViolation("H264 parameter sets exceed explicit byte limits"));
    }
    let mut has_sps = false;
    let mut has_pps = false;
    let mut offset = 0;
    while offset < config.avcc.sps_pps.len() {
        if !config.avcc.sps_pps[offset..].starts_with(&[0, 0, 0, 1]) {
            return Err(RtspError::ProtocolViolation("invalid H264 parameter-set framing"));
        }
        let start = offset + 4;
        let end = config.avcc.sps_pps[start..]
            .windows(4)
            .position(|window| window == [0, 0, 0, 1])
            .map(|relative| start + relative)
            .unwrap_or(config.avcc.sps_pps.len());
        let nal = &config.avcc.sps_pps[start..end];
        if nal.is_empty() || nal[0] & 0x80 != 0 {
            return Err(RtspError::ProtocolViolation("invalid H264 parameter set"));
        }
        match nal[0] & 0x1f {
            7 if nal.len() >= 4 => {
                has_sps = true;
                if nal[1] != limits.profile_idc || nal[3] != limits.level_idc {
                    return Err(RtspError::ProtocolViolation("H264 profile or level exceeds explicit target policy"));
                }
            }
            8 if nal.len() >= 2 => has_pps = true,
            _ => return Err(RtspError::ProtocolViolation("config must contain only valid H264 SPS and PPS")),
        }
        offset = end;
    }
    if !has_sps || !has_pps {
        return Err(RtspError::ProtocolViolation("H264 config requires both SPS and PPS"));
    }
    Ok(())
}

/// Validate the Annex B access unit produced by `ScreenFrame::video_decode`,
/// rejecting in-band parameter-set changes and detecting IDR across the unit.
fn validate_access_unit(data: &[u8]) -> RtspResult<bool> {
    if data.is_empty() {
        return Err(RtspError::ProtocolViolation("empty H264 access unit"));
    }
    let mut offset = 0usize;
    let mut has_idr = false;
    let mut has_delta = false;
    let mut nal_count = 0;
    while offset < data.len() {
        if !data[offset..].starts_with(&[0, 0, 0, 1]) {
            return Err(RtspError::ProtocolViolation("invalid Annex B NAL start code"));
        }
        nal_count += 1;
        if nal_count > MAX_ACCESS_UNIT_NALS {
            return Err(RtspError::ProtocolViolation("too many H264 NALs in access unit"));
        }
        let nal_start = offset + 4;
        let nal_end = data[nal_start..]
            .windows(4)
            .position(|window| window == [0, 0, 0, 1])
            .map(|relative| nal_start + relative)
            .unwrap_or(data.len());
        if nal_end <= nal_start {
            return Err(RtspError::ProtocolViolation("empty H264 NAL"));
        }
        let nal_header = data[nal_start];
        if nal_header & 0x80 != 0 {
            return Err(RtspError::ProtocolViolation("invalid H264 NAL header"));
        }
        match nal_header & 0x1f {
            1 => has_delta = true,
            5 if nal_header & 0x60 != 0 && nal_end - nal_start > 1 => has_idr = true,
            5 => return Err(RtspError::ProtocolViolation("invalid or empty H264 IDR NAL")),
            6 | 9 | 12 if nal_header & 0x60 == 0 => {}
            7 | 8 => return Err(RtspError::ProtocolViolation("in-band H264 SPS/PPS changes are not supported")),
            _ => return Err(RtspError::ProtocolViolation("unsupported H264 NAL type")),
        }
        offset = nal_end;
    }
    if has_idr && has_delta {
        return Err(RtspError::ProtocolViolation("mixed IDR and non-IDR slices in H264 access unit"));
    }
    Ok(has_idr)
}

fn frame_retained_bytes(frame: &EncodedVideoFrame) -> Option<usize> {
    let mut bytes = frame.data.capacity();
    bytes = bytes.checked_add(frame.header_buf.capacity())?;
    bytes = bytes.checked_add(frame.chacha_tag_buf.capacity())?;
    if let Some(offsets) = frame.nal_offsets.as_ref() {
        bytes = bytes.checked_add(
            offsets
                .capacity()
                .checked_mul(std::mem::size_of::<catplay_carplay::video::NalChunk>())?,
        )?;
    }
    if let Some(config) = frame.config.as_ref() {
        bytes = bytes.checked_add(config.avcc.sps_pps.capacity())?;
    }
    Some(bytes)
}

fn advance_revision(state: &mut BridgeState) {
    state.revision = state
        .revision
        .checked_add(1)
        .expect("mirror bridge revision space exhausted");
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_carplay::{
        clock::{MediaClock, NtpU64},
        screen::ScreenFrame,
        video::{AvccConfig, Pts},
    };
    use std::time::Instant;

    struct Clock(Instant);
    impl MediaClock for Clock {
        fn is_synchronized(&self) -> bool {
            true
        }
        fn decode_remote(&self, pts: NtpU64) -> Option<Instant> {
            Some(self.decode_local(pts))
        }
        fn decode_local(&self, pts: NtpU64) -> Instant {
            self.0 + Duration::from_nanos(pts.as_nanos() as u64)
        }
        fn encode_local(&self, pts: Instant) -> NtpU64 {
            NtpU64::from_monotonic_nanos(pts.duration_since(self.0).as_nanos() as i128)
        }
        fn encode_remote(&self, pts: Instant) -> Option<NtpU64> {
            Some(self.encode_local(pts))
        }
    }

    fn limits() -> MirrorLimits {
        MirrorLimits::new(800, 480, 66, 30, 1024).unwrap()
    }

    fn config() -> AvccConfigExtended {
        AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: vec![0, 0, 0, 1, 0x67, 66, 0, 30, 0, 0, 0, 1, 0x68, 0],
            },
            video_latency: Duration::ZERO,
            width: 800,
            height: 480,
            respect_timestamps: true,
        }
    }

    fn frame(nals: &[&[u8]]) -> EncodedVideoFrame {
        let mut data = BytesMut::new();
        for nal in nals {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(nal);
        }
        EncodedVideoFrame {
            pts: Pts(Instant::now()),
            width: 800,
            height: 480,
            data,
            config: Some(config()),
            nal_offsets: None,
            is_keyframe: Some(false),
            header_buf: BytesMut::new(),
            chacha_tag_buf: BytesMut::new(),
        }
    }

    #[tokio::test]
    async fn forwards_only_validated_idr_and_attaches_config() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config()).await.unwrap();
        assert!(sink.process_frame(frame(&[&[0x41, 1]])).await.is_err());
        sink.process_frame(frame(&[&[0x09, 0xf0], &[0x06, 0x80], &[0x65, 0x80]]))
            .await
            .unwrap();
        let forwarded = receiver.take_frame().unwrap();
        assert_eq!(forwarded.is_keyframe, Some(true));
        assert_eq!(forwarded.config, Some(config()));
        assert!(forwarded.nal_offsets.is_none());
    }

    #[tokio::test]
    async fn reconfiguration_discards_pending_frame_and_requires_new_idr() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(&[&[0x65, 0x80]])).await.unwrap();
        let changed = AvccConfigExtended {
            video_latency: Duration::from_millis(10),
            ..config()
        };
        sink.set_avcc_config(changed.clone()).await.unwrap();
        assert!(receiver.take_frame().is_none());
        assert!(sink.process_frame(frame(&[&[0x41, 0x80]])).await.is_err());
        let mut fresh = frame(&[&[0x65, 0x80]]);
        fresh.config = Some(changed.clone());
        sink.process_frame(fresh).await.unwrap();
        assert_eq!(receiver.take_frame().unwrap().config, Some(changed));
    }

    #[test]
    fn validates_every_nal_and_rejects_in_band_parameter_sets() {
        assert!(!validate_access_unit(&[0, 0, 0, 1, 0x41, 1]).unwrap());
        assert!(validate_access_unit(&[0, 0, 0, 1, 0x41, 1, 0, 0, 0, 1, 0x65, 0x80]).is_err());
        assert!(validate_access_unit(&[0, 0, 0, 1, 0x67, 66, 0, 30]).is_err());
        assert!(validate_access_unit(&[0, 0, 0, 1, 0x65, 0x80]).unwrap());
        assert!(validate_access_unit(&[0, 0, 0, 1, 0x65]).is_err());
    }

    #[tokio::test]
    async fn real_screen_decode_bridge_and_transmit_conversion_keep_annexb_payload() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        let config = config();
        sink.set_avcc_config(config.clone()).await.unwrap();
        let clock = Clock(Instant::now());
        let source_nals: [&[u8]; 3] = [&[0x09, 0xf0], &[0x06, 0x80], &[0x65, 0x80]];
        let mut avcc = BytesMut::new();
        let mut annexb = BytesMut::new();
        for nal in source_nals {
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(nal);
        }
        let decoded = ScreenFrame::video_avcc(avcc, clock.0, &clock)
            .video_decode(&config, &clock)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.data, annexb);
        sink.process_frame(decoded).await.unwrap();
        let bridged = receiver.take_frame().unwrap();
        let tx_frame = ScreenFrame::video_proxied(bridged, 4, &clock).unwrap();
        let decoded_at_tx = tx_frame.video_decode(&config, &clock).unwrap().unwrap();
        assert_eq!(decoded_at_tx.data, annexb);
        assert_eq!(decoded_at_tx.is_keyframe, Some(true));
        assert_eq!(decoded_at_tx.config, Some(config));
    }

    #[tokio::test]
    async fn closing_generation_rejects_late_producer_callbacks() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config()).await.unwrap();
        receiver.close();
        assert!(sink.process_frame(frame(&[&[0x65, 0x80]])).await.is_err());
        assert!(receiver.is_closed());
    }

    #[tokio::test]
    async fn rejected_reconfiguration_clears_old_stream_and_invalidates_taken_frame() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(&[&[0x65, 0x80]])).await.unwrap();
        let (revision, _frame) = receiver.take_frame_tagged().unwrap();
        let invalid = AvccConfigExtended { width: 640, ..config() };
        assert!(sink.set_avcc_config(invalid).await.is_err());
        assert!(!receiver.is_revision_current(revision));
        assert!(sink.process_frame(frame(&[&[0x65, 0x80]])).await.is_err());
    }
}
