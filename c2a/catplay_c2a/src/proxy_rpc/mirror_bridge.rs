//! Bounded, picture-only AirPlay to CarPlay video bridge.
//!
//! The limits are supplied by the caller from explicit receiver/vehicle
//! settings. This module does not infer decoder support from observed streams.

use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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
pub const MIRROR_IDR_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
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
            idr_wait: Some(IdrWait {
                kind: MirrorIdrWaitState::WaitingInitialIdr,
                deadline: Instant::now() + MIRROR_IDR_WAIT_TIMEOUT,
            }),
        }));
        let producer_notify = Arc::new(Notify::new());
        let consumer_notify = Arc::new(Notify::new());
        (
            MirrorBridgeSink {
                state: state.clone(),
                producer_notify: producer_notify.clone(),
                consumer_notify: consumer_notify.clone(),
                config_summary_emitted: false,
                diagnostics_enabled: false,
                first_accepted_video_frame_reported: false,
                first_accepted_idr_reported: false,
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
    idr_wait: Option<IdrWait>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorIdrWaitState {
    WaitingInitialIdr,
    Streaming,
    WaitingRecoveryIdr,
    Closed,
}

struct IdrWait {
    kind: MirrorIdrWaitState,
    deadline: Instant,
}

/// AirPlay screen sink that validates each access unit and retains at most one
/// encoded frame for the connected-car event loop.
pub struct MirrorBridgeSink {
    state: Arc<Mutex<BridgeState>>,
    producer_notify: Arc<Notify>,
    consumer_notify: Arc<Notify>,
    config_summary_emitted: bool,
    diagnostics_enabled: bool,
    first_accepted_video_frame_reported: bool,
    first_accepted_idr_reported: bool,
}

/// Consumer side owned by the connected-car event loop.
pub struct MirrorBridgeReceiver {
    state: Arc<Mutex<BridgeState>>,
    producer_notify: Arc<Notify>,
    consumer_notify: Arc<Notify>,
}

/// Weak liveness/status handle retained by the AirPlay session. It does not
/// keep a closed screen bridge or its queue alive.
#[derive(Clone)]
pub struct MirrorStreamStatus {
    state: Weak<Mutex<BridgeState>>,
}

impl MirrorStreamStatus {
    pub fn is_closed(&self) -> bool {
        self.state
            .upgrade()
            .is_none_or(|state| state.lock().unwrap().closed)
    }

    pub fn idr_wait_state(&self) -> MirrorIdrWaitState {
        let Some(state) = self.state.upgrade() else {
            return MirrorIdrWaitState::Closed;
        };
        idr_wait_state(&state.lock().unwrap())
    }
}

impl MirrorBridgeReceiver {
    pub fn generation(&self) -> usize {
        self.state.lock().unwrap().generation
    }

    pub fn stream_status(&self) -> MirrorStreamStatus {
        MirrorStreamStatus {
            state: Arc::downgrade(&self.state),
        }
    }

    pub fn idr_wait_state(&self) -> MirrorIdrWaitState {
        idr_wait_state(&self.state.lock().unwrap())
    }

    /// True after the initial or recovery IDR deadline expires.
    pub fn recovery_expired(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.needs_idr
            && state
                .idr_wait
                .as_ref()
                .is_some_and(|wait| Instant::now() >= wait.deadline)
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
            if frame.is_known_keyframe() && frame.config.is_some() {
                state.needs_idr = false;
                state.idr_wait = None;
            }
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
        if state.closed {
            return;
        }
        require_idr(&mut state);
        advance_revision(&mut state);
        if state
            .pending
            .as_ref()
            .is_some_and(|frame| frame.is_known_keyframe() && frame.config.is_some())
        {
            state.pending_revision = state.revision;
        } else {
            state.pending.take();
        }
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
        state.idr_wait = None;
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
        state.idr_wait = None;
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
            require_idr(&mut state);
            advance_revision(&mut state);
            self.producer_notify.notify_one();
            self.consumer_notify.notify_one();
            state.limits
        };
        if self.diagnostics_enabled && !self.config_summary_emitted {
            eprintln!("{}", config_summary_line(&config, limits));
            self.config_summary_emitted = true;
        }
        if let Err(err) = validate_config(&config, limits) {
            return Err(err);
        }

        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(closed_error());
        }
        state.config = Some(config);
        require_idr(&mut state);
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
            if state.needs_idr
                && state
                    .idr_wait
                    .as_ref()
                    .is_some_and(|wait| Instant::now() >= wait.deadline)
            {
                return Err(RtspError::ProtocolViolation("timed out waiting for mirror IDR"));
            }
            (state.limits, state.needs_idr, state.revision)
        };
        if frame.width != limits.width || frame.height != limits.height || frame.data.len() > limits.max_frame_bytes {
            return Err(RtspError::ProtocolViolation("mirror frame exceeds explicit target limits"));
        }
        let is_idr = validate_access_unit(&frame.data)?;
        if needs_idr && !is_idr {
            return Ok(());
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
        let mut frame = Some(frame);
        loop {
            // Register before checking the slot to avoid missing a consumer wake.
            let notified = self.producer_notify.notified();
            let accepted = {
                let mut state = self.state.lock().unwrap();
                if state.closed {
                    return Err(closed_error());
                }
                if state.revision != accepted_revision {
                    return Ok(());
                }
                if is_idr && state.config.as_ref() != frame.as_ref().and_then(|frame| frame.config.as_ref()) {
                    return Err(RtspError::ProtocolViolation("stale mirror generation config"));
                }
                if state.needs_idr && !is_idr {
                    return Ok(());
                }
                if state.pending.is_none() {
                    let Some(frame) = frame.take() else {
                        return Ok(());
                    };
                    state.pending = Some(frame);
                    state.pending_revision = state.revision;
                    state.needs_idr = false;
                    state.idr_wait = None;
                    self.consumer_notify.notify_one();
                    let report_video = self.diagnostics_enabled && !self.first_accepted_video_frame_reported;
                    let report_idr = self.diagnostics_enabled && is_idr && !self.first_accepted_idr_reported;
                    self.first_accepted_video_frame_reported |= report_video;
                    self.first_accepted_idr_reported |= report_idr;
                    Some((report_video, report_idr))
                } else {
                    None
                }
            };
            if let Some((report_video, report_idr)) = accepted {
                if report_video {
                    eprintln!("first_accepted_video_frame");
                }
                if report_idr {
                    eprintln!("first_accepted_idr");
                }
                return Ok(());
            }
            if tokio::time::timeout(PUBLISH_TIMEOUT, notified)
                .await
                .is_err()
            {
                let mut state = self.state.lock().unwrap();
                if state.closed {
                    return Err(closed_error());
                }
                if state.revision != accepted_revision {
                    return Ok(());
                }
                require_idr(&mut state);
                state.pending.take();
                advance_revision(&mut state);
                self.producer_notify.notify_one();
                self.consumer_notify.notify_one();
                return Ok(());
            }
        }
    }
}

impl MirrorBridgeSink {
    pub fn with_experimental_diagnostics(mut self) -> Self {
        self.diagnostics_enabled = true;
        self
    }

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
        state.idr_wait = None;
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
        state.idr_wait = None;
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

fn require_idr(state: &mut BridgeState) {
    if state.idr_wait.is_some() {
        return;
    }
    let kind = if state.needs_idr {
        MirrorIdrWaitState::WaitingInitialIdr
    } else {
        MirrorIdrWaitState::WaitingRecoveryIdr
    };
    state.needs_idr = true;
    state.idr_wait = Some(IdrWait {
        kind,
        deadline: Instant::now() + MIRROR_IDR_WAIT_TIMEOUT,
    });
}

fn idr_wait_state(state: &BridgeState) -> MirrorIdrWaitState {
    if state.closed {
        MirrorIdrWaitState::Closed
    } else {
        state
            .idr_wait
            .as_ref()
            .map(|wait| wait.kind)
            .unwrap_or(if state.needs_idr {
                MirrorIdrWaitState::WaitingInitialIdr
            } else {
                MirrorIdrWaitState::Streaming
            })
    }
}

fn first_sps_header(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let mut offset = 0usize;
    while offset.checked_add(4)? <= bytes.len() {
        if !bytes[offset..].starts_with(&[0, 0, 0, 1]) {
            return None;
        }
        let start = offset + 4;
        let end = bytes[start..]
            .windows(4)
            .position(|window| window == [0, 0, 0, 1])
            .map(|relative| start + relative)
            .unwrap_or(bytes.len());
        let nal = &bytes[start..end];
        if nal.first().is_some_and(|header| header & 0x80 == 0 && header & 0x1f == 7) {
            return Some((*nal.get(1)?, *nal.get(2)?, *nal.get(3)?));
        }
        offset = end;
    }
    None
}

fn config_summary_line(config: &AvccConfigExtended, limits: MirrorLimits) -> String {
    let (profile, constraints, level) = first_sps_header(&config.avcc.sps_pps)
        .map(|(profile, constraints, level)| (Some(profile), Some(constraints), Some(level)))
        .unwrap_or((None, None, None));
    format!(
        "mirror_config observed_width={} observed_height={} hevc={} nal_size_len={} sps_profile_idc={profile:?} sps_constraint_flags={constraints:?} sps_level_idc={level:?} expected_width={} expected_height={} expected_profile_idc={} expected_level_idc={}",
        config.width,
        config.height,
        config.hevc,
        config.avcc.nal_size_len,
        limits.width,
        limits.height,
        limits.profile_idc,
        limits.level_idc,
    )
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
        screen::{
            ScreenFrame,
            tx::{ScreenTransmitOp, ScreenTransmitProxy, ScreenTransmitSink},
        },
        video::{AvccConfig, Pts},
    };
    use catplay_util::notify::Notify;
    use std::{collections::VecDeque, time::Instant};

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

    #[test]
    fn first_sps_header_and_config_summary_are_numeric_and_bounded() {
        let config = config();
        assert_eq!(first_sps_header(&config.avcc.sps_pps), Some((66, 0, 30)));
        assert_eq!(first_sps_header(&[0, 0, 0, 1, 0x68, 0]), None);
        let line = config_summary_line(&config, limits());
        assert_eq!(
            line,
            "mirror_config observed_width=800 observed_height=480 hevc=false nal_size_len=4 sps_profile_idc=Some(66) sps_constraint_flags=Some(0) sps_level_idc=Some(30) expected_width=800 expected_height=480 expected_profile_idc=66 expected_level_idc=30"
        );
        assert!(line.len() <= 256);
        assert!(!line.contains("0x67"));
        assert!(!line.contains("0x68"));
    }

    #[tokio::test]
    async fn config_summary_is_emitted_at_most_once_per_bridge_stream() {
        let (sink, _receiver) = MirrorBridge::new(limits());
        let mut sink = sink.with_experimental_diagnostics();
        assert!(!sink.config_summary_emitted);
        sink.set_avcc_config(config()).await.unwrap();
        assert!(sink.config_summary_emitted);
        sink.set_avcc_config(config()).await.unwrap();
        assert!(sink.config_summary_emitted);
    }

    #[tokio::test]
    async fn first_frame_and_idr_diagnostics_follow_bridge_admission_once() {
        let (sink, mut receiver) = MirrorBridge::new(limits());
        let mut sink = sink.with_experimental_diagnostics();
        sink.set_avcc_config(config()).await.unwrap();
        assert!(!sink.first_accepted_video_frame_reported);
        assert!(!sink.first_accepted_idr_reported);
        sink.process_frame(frame(&[&[0x41, 0x80]])).await.unwrap();
        assert!(!sink.first_accepted_video_frame_reported);
        assert!(!sink.first_accepted_idr_reported);
        sink.process_frame(frame(&[&[0x65, 0x80]])).await.unwrap();
        assert!(sink.first_accepted_video_frame_reported);
        assert!(sink.first_accepted_idr_reported);
        assert!(receiver.take_frame().is_some());
        sink.process_frame(frame(&[&[0x65, 0x80]])).await.unwrap();
        assert!(sink.first_accepted_video_frame_reported);
        assert!(sink.first_accepted_idr_reported);
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
        sink.process_frame(frame(&[&[0x41, 1]])).await.unwrap();
        assert!(receiver.take_frame().is_none());
        assert_eq!(receiver.idr_wait_state(), MirrorIdrWaitState::WaitingInitialIdr);
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
        assert!(matches!(
            sink.process_frame(frame(&[&[0x41, 0x80]])).await,
            Err(RtspError::ProtocolViolation("mirror frame config changed without reconfiguration"))
        ));
        let mut delta = frame(&[&[0x41, 0x80]]);
        delta.config = Some(changed.clone());
        sink.process_frame(delta).await.unwrap();
        assert!(receiver.take_frame().is_none());
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
    async fn decoded_screen_recovers_through_bridge_and_transmitter_on_natural_idr() {
        let config = config();
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config.clone()).await.unwrap();
        let clock = Clock(Instant::now());
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let mut tx = ScreenTransmitProxy::new(
            Duration::ZERO,
            queue.clone(),
            Arc::new(Mutex::new(false)),
            Notify::new(),
            Notify::new(),
            4,
        );
        let decode = |nal: &[u8]| {
            let mut avcc = BytesMut::new();
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
            ScreenFrame::video_avcc(avcc, clock.0, &clock)
                .video_decode(&config, &clock)
                .unwrap()
                .unwrap()
        };

        sink.process_frame(decode(&[0x65, 0x80])).await.unwrap();
        tx.push_frame(receiver.take_frame().unwrap()).await.unwrap();
        tx.reset_for_keyframe();
        receiver.invalidate_until_idr();
        assert!(queue.lock().unwrap().is_empty());

        sink.process_frame(decode(&[0x41, 0x80])).await.unwrap();
        assert!(receiver.take_frame().is_none());
        assert_eq!(tx.queue_len(), 0);
        assert!(tx.needs_keyframe());

        sink.process_frame(decode(&[0x65, 0x80])).await.unwrap();
        tx.push_frame(receiver.take_frame().unwrap()).await.unwrap();
        let ops = queue.lock().unwrap();
        assert!(matches!(ops.front(), Some(ScreenTransmitOp::Configure(_))));
        assert!(matches!(ops.get(1), Some(ScreenTransmitOp::Frame(frame)) if frame.is_known_keyframe()));
        assert_eq!(ops.len(), 2);
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

    #[tokio::test]
    async fn initial_idr_wait_drops_deltas_and_repeated_config_does_not_extend_deadline() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        let deadline = receiver
            .state
            .lock()
            .unwrap()
            .idr_wait
            .as_ref()
            .unwrap()
            .deadline;
        sink.set_avcc_config(config()).await.unwrap();
        for _ in 0..3 {
            sink.process_frame(frame(&[&[0x41, 0x80]])).await.unwrap();
        }
        let changed_latency = AvccConfigExtended {
            video_latency: Duration::from_millis(20),
            ..config()
        };
        sink.set_avcc_config(changed_latency).await.unwrap();
        assert_eq!(
            receiver
                .state
                .lock()
                .unwrap()
                .idr_wait
                .as_ref()
                .unwrap()
                .deadline,
            deadline
        );
        assert_eq!(receiver.idr_wait_state(), MirrorIdrWaitState::WaitingInitialIdr);
        assert!(receiver.take_frame().is_none());
    }

    #[test]
    fn initial_idr_deadline_expires_even_before_config_or_frames() {
        let (_sink, receiver) = MirrorBridge::new(limits());
        assert_eq!(receiver.idr_wait_state(), MirrorIdrWaitState::WaitingInitialIdr);
        assert!(!receiver.recovery_expired());
        receiver
            .state
            .lock()
            .unwrap()
            .idr_wait
            .as_mut()
            .unwrap()
            .deadline = Instant::now() - Duration::from_millis(1);
        assert!(receiver.recovery_expired());
    }

    #[tokio::test]
    async fn recovery_deadline_starts_once_and_expires_without_new_frames() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(&[&[0x65, 0x80]])).await.unwrap();
        receiver.take_frame().unwrap();
        assert_eq!(receiver.idr_wait_state(), MirrorIdrWaitState::Streaming);

        receiver.invalidate_until_idr();
        let deadline = receiver
            .state
            .lock()
            .unwrap()
            .idr_wait
            .as_ref()
            .unwrap()
            .deadline;
        assert_eq!(receiver.idr_wait_state(), MirrorIdrWaitState::WaitingRecoveryIdr);
        receiver.invalidate_until_idr();
        sink.set_avcc_config(config()).await.unwrap();
        assert_eq!(
            receiver
                .state
                .lock()
                .unwrap()
                .idr_wait
                .as_ref()
                .unwrap()
                .deadline,
            deadline
        );
        assert!(!receiver.recovery_expired());

        receiver
            .state
            .lock()
            .unwrap()
            .idr_wait
            .as_mut()
            .unwrap()
            .deadline = Instant::now() - Duration::from_millis(1);
        assert!(receiver.recovery_expired());
        assert!(sink.process_frame(frame(&[&[0x41, 0x80]])).await.is_err());
    }

    #[tokio::test]
    async fn invalidation_keeps_pending_configured_idr_and_revisions_it() {
        let (mut sink, mut receiver) = MirrorBridge::new(limits());
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(&[&[0x65, 0x80]])).await.unwrap();
        let previous_revision = receiver.revision();
        receiver.invalidate_until_idr();
        let (revision, idr) = receiver.take_frame_tagged().unwrap();
        assert!(revision > previous_revision);
        assert_eq!(idr.config, Some(config()));
        assert_eq!(receiver.idr_wait_state(), MirrorIdrWaitState::Streaming);
    }

    #[tokio::test]
    async fn weak_status_reports_sink_shutdown_and_does_not_keep_state_alive() {
        let (mut sink, receiver) = MirrorBridge::new(limits());
        let status = receiver.stream_status();
        assert!(!status.is_closed());
        sink.shutdown().await;
        assert!(status.is_closed());
        assert_eq!(status.idr_wait_state(), MirrorIdrWaitState::Closed);
        drop(receiver);
        drop(sink);
        assert!(status.is_closed());
    }
}
