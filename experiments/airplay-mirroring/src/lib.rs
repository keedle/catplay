//! Host-only, picture-only admission experiment. This does not receive AirPlay,
//! decrypt media, decode H.264, connect to a car, or implement CarPlay ownership.
//!
//! The adapter must validate the observed coded size/cropping, SPS/PPS semantics,
//! reference structure and head-unit support before asserting a descriptor here.
//! This core checks that descriptor against supplied caps and checks NAL framing.
//! Header checks alone do not prove that an H.264 decoder can decode the stream.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

static NEXT_POLICY_ID: AtomicUsize = AtomicUsize::new(1);

fn allocate_policy_id() -> Result<usize, Refusal> {
    let mut current = NEXT_POLICY_ID.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).ok_or(Refusal::GenerationExhausted)?;
        match NEXT_POLICY_ID.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CarPlay,
    Mirroring,
}

/// Explicit host experiment opt-in. The default does not permit mirror leases.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MirroringOptIn {
    #[default]
    Disabled,
    Enabled,
}

/// An ownership token. Its private fields prevent callers from fabricating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    policy_id: usize,
    generation: u64,
    source: Source,
}

impl Lease {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn source(self) -> Source {
        self.source
    }
}

/// Bounds on memory owned by this policy, not on a downstream consumer's copies.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub max_parameter_set_bytes: usize,
    pub max_nals_per_frame: usize,
    pub max_queued_frames: usize,
    /// Retained parameter sets plus all queued compressed-frame bytes.
    pub max_buffered_bytes: usize,
}

/// Explicitly observed/validated receiver capability, not a universal car preset.
#[derive(Debug, Clone, Copy)]
pub struct CarDisplayCaps {
    pub width: u32,
    pub height: u32,
    /// Subset of supported profile_idc values: Baseline 66, Main 77, High 100.
    pub h264_profiles: &'static [u8],
    pub max_level_idc: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Avcc4,
    AnnexB,
}

/// Caller assertion from an independently validated stream observation.
///
/// The core verifies SPS profile/constraint/level header bytes, not its Exp-Golomb
/// fields, coded dimensions, cropping, VUI or the PPS-to-SPS relationship.
#[derive(Debug, Clone, Copy)]
pub struct ObservedConfig<'a> {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub rotation_degrees: u16,
    pub framing: Framing,
    /// Raw NAL units, including NAL header, with no start code/length prefix.
    pub sps: &'a [u8],
    pub pps: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub sps: Box<[u8]>,
    pub pps: Box<[u8]>,
}

impl VideoConfig {
    pub fn payload_bytes(&self) -> usize {
        self.sps.len() + self.pps.len()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct VideoFrame {
    /// Opaque adapter timestamp, preserved exactly. No clock translation,
    /// monotonicity or reordering policy is claimed by this host-only core.
    pub timestamp: u64,
    pub is_idr: bool,
    /// Original AVCC4 compressed payload; no pixel conversion occurs.
    pub data: Box<[u8]>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Packet {
    Config(Arc<VideoConfig>),
    Frame(VideoFrame),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    InvalidLimits,
    InvalidCaps,
    MirroringDisabled,
    SourceBusy,
    StaleLease,
    GenerationExhausted,
    NotMirroring,
    UnsupportedCodec,
    UnsupportedFraming,
    UnsupportedRotation,
    DimensionsMismatch,
    UnsupportedProfile,
    UnsupportedLevel,
    InvalidParameterSets,
    ParameterSetsTooLarge,
    NeedConfig,
    NeedKeyframe,
    FrameTooLarge,
    EmptyFrame,
    InvalidNalLength,
    InvalidNalHeader,
    UnsupportedNalType,
    TooManyNals,
    MixedSliceTypes,
    QueueFull,
    MirrorAudioUnsupported,
    MirrorControlUnsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    CarPlayOwned,
    AwaitingConfig,
    AwaitingKeyframe,
    Streaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub owner: Option<Lease>,
    pub phase: Phase,
    pub queued_frames: usize,
    pub queued_packets: usize,
    pub buffered_payload_bytes: usize,
}

struct Session {
    lease: Lease,
    config: Option<Arc<VideoConfig>>,
    awaiting_keyframe: bool,
    queue: VecDeque<Packet>,
    queued_frames: usize,
    queued_frame_bytes: usize,
}

pub struct ReceiverPolicy {
    limits: Limits,
    caps: CarDisplayCaps,
    opt_in: MirroringOptIn,
    generation: u64,
    policy_id: usize,
    session: Option<Session>,
}

impl ReceiverPolicy {
    pub fn new(limits: Limits, caps: CarDisplayCaps, opt_in: MirroringOptIn) -> Result<Self, Refusal> {
        if limits.max_frame_bytes == 0
            || limits.max_parameter_set_bytes < 6
            || limits.max_nals_per_frame == 0
            || limits.max_queued_frames == 0
            || limits
                .max_frame_bytes
                .checked_add(limits.max_parameter_set_bytes)
                .is_none_or(|minimum| minimum > limits.max_buffered_bytes)
        {
            return Err(Refusal::InvalidLimits);
        }
        if caps.width == 0
            || caps.height == 0
            || caps.h264_profiles.is_empty()
            || caps
                .h264_profiles
                .iter()
                .any(|p| !matches!(p, 66 | 77 | 100))
            || !matches!(
                caps.max_level_idc,
                10 | 11 | 12 | 13 | 20 | 21 | 22 | 30 | 31 | 32 | 40 | 41 | 42 | 50 | 51 | 52
            )
        {
            return Err(Refusal::InvalidCaps);
        }
        let policy_id = allocate_policy_id()?;
        Ok(Self {
            limits,
            caps,
            opt_in,
            generation: 0,
            policy_id,
            session: None,
        })
    }

    /// Availability defaults to CarPlay; no live stream is fabricated on release.
    pub fn default_source(&self) -> Source {
        Source::CarPlay
    }

    pub fn acquire(&mut self, source: Source) -> Result<Lease, Refusal> {
        if source == Source::Mirroring && self.opt_in == MirroringOptIn::Disabled {
            return Err(Refusal::MirroringDisabled);
        }
        if self.session.is_some() {
            return Err(Refusal::SourceBusy);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(Refusal::GenerationExhausted)?;
        let lease = Lease {
            policy_id: self.policy_id,
            generation,
            source,
        };
        self.generation = generation;
        self.session = Some(Session {
            lease,
            config: None,
            awaiting_keyframe: true,
            queue: VecDeque::new(),
            queued_frames: 0,
            queued_frame_bytes: 0,
        });
        Ok(lease)
    }

    pub fn release(&mut self, lease: Lease) -> Result<(), Refusal> {
        self.check_lease(lease)?;
        self.session = None;
        Ok(())
    }

    fn check_lease(&self, lease: Lease) -> Result<(), Refusal> {
        if self.session.as_ref().is_some_and(|s| s.lease == lease) {
            Ok(())
        } else {
            Err(Refusal::StaleLease)
        }
    }

    fn check_mirror(&self, lease: Lease) -> Result<(), Refusal> {
        self.check_lease(lease)?;
        if lease.source != Source::Mirroring {
            return Err(Refusal::NotMirroring);
        }
        Ok(())
    }

    pub fn configure(&mut self, lease: Lease, config: ObservedConfig<'_>) -> Result<(), Refusal> {
        self.check_mirror(lease)?;
        // A rejected reconfiguration must not leave old deltas eligible for output.
        if let Err(error) = self.validate_config(config) {
            let session = self.session.as_mut().unwrap();
            session.config = None;
            session.queue.clear();
            session.queued_frames = 0;
            session.queued_frame_bytes = 0;
            session.awaiting_keyframe = true;
            return Err(error);
        }
        // Drop the old queue/config before allocating the replacement, so even
        // reconfiguration does not temporarily retain two payload budgets.
        let session = self.session.as_mut().unwrap();
        session.queue.clear();
        session.config = None;
        session.queued_frames = 0;
        session.queued_frame_bytes = 0;
        session.awaiting_keyframe = true;
        let config = Arc::new(VideoConfig {
            width: config.width,
            height: config.height,
            profile_idc: config.profile_idc,
            constraint_flags: config.constraint_flags,
            level_idc: config.level_idc,
            sps: config.sps.into(),
            pps: config.pps.into(),
        });
        let session = self.session.as_mut().unwrap();
        session.config = Some(config);
        Self::require_keyframe(session);
        Ok(())
    }

    fn validate_config(&self, config: ObservedConfig<'_>) -> Result<(), Refusal> {
        if config.codec != Codec::H264 {
            return Err(Refusal::UnsupportedCodec);
        }
        if config.framing != Framing::Avcc4 {
            return Err(Refusal::UnsupportedFraming);
        }
        if config.rotation_degrees != 0 {
            return Err(Refusal::UnsupportedRotation);
        }
        if config.width == 0 || config.height == 0 || config.width != self.caps.width || config.height != self.caps.height {
            return Err(Refusal::DimensionsMismatch);
        }
        if !self.caps.h264_profiles.contains(&config.profile_idc) {
            return Err(Refusal::UnsupportedProfile);
        }
        // Level 1b and levels above the configured receiver cap are outside this POC.
        if !matches!(
            config.level_idc,
            10 | 11 | 12 | 13 | 20 | 21 | 22 | 30 | 31 | 32 | 40 | 41 | 42 | 50 | 51 | 52
        ) || config.level_idc > self.caps.max_level_idc
            || (config.level_idc == 11 && config.constraint_flags & 0x10 != 0)
        {
            return Err(Refusal::UnsupportedLevel);
        }
        if config
            .sps
            .len()
            .checked_add(config.pps.len())
            .is_none_or(|size| size > self.limits.max_parameter_set_bytes)
        {
            return Err(Refusal::ParameterSetsTooLarge);
        }
        if config.sps.len() < 4
            || config.pps.len() < 2
            || config.sps[0] & 0x80 != 0
            || config.sps[0] & 0x1f != 7
            || config.pps[0] & 0x80 != 0
            || config.pps[0] & 0x1f != 8
            || config.sps[0] & 0x60 == 0
            || config.pps[0] & 0x60 == 0
            || config.constraint_flags & 3 != 0
            || config.sps[1..4] != [config.profile_idc, config.constraint_flags, config.level_idc]
        {
            return Err(Refusal::InvalidParameterSets);
        }
        Ok(())
    }

    /// Borrowed input is validated before the single bounded payload allocation.
    /// This checks NAL headers/framing, not access-unit or slice decode validity.
    /// In-band SPS/PPS are rejected, including copies of the accepted config.
    /// timestamp is opaque and may go backwards; this core does not map clocks.
    pub fn push_avcc_frame(&mut self, lease: Lease, timestamp: u64, data: &[u8]) -> Result<(), Refusal> {
        self.check_mirror(lease)?;
        if self.session.as_ref().unwrap().config.is_none() {
            return Err(Refusal::NeedConfig);
        }
        let is_idr = match inspect_access_unit(data, self.limits) {
            Ok(is_idr) => is_idr,
            Err(error) => {
                Self::require_keyframe(self.session.as_mut().unwrap());
                return Err(error);
            }
        };
        let session = self.session.as_mut().unwrap();
        if session.awaiting_keyframe && !is_idr {
            return Err(Refusal::NeedKeyframe);
        }
        let config_bytes = session.config.as_ref().unwrap().payload_bytes();
        let next_bytes = session
            .queued_frame_bytes
            .checked_add(data.len())
            .and_then(|n| n.checked_add(config_bytes));
        if session.queued_frames >= self.limits.max_queued_frames || next_bytes.is_none_or(|n| n > self.limits.max_buffered_bytes) {
            Self::require_keyframe(session);
            return Err(Refusal::QueueFull);
        }
        session.queue.push_back(Packet::Frame(VideoFrame {
            timestamp,
            is_idr,
            data: data.into(),
        }));
        session.queued_frames += 1;
        session.queued_frame_bytes += data.len();
        session.awaiting_keyframe = false;
        Ok(())
    }

    fn require_keyframe(session: &mut Session) {
        session.queue.clear();
        session.queued_frames = 0;
        session.queued_frame_bytes = 0;
        session.awaiting_keyframe = true;
        if let Some(config) = session.config.as_ref() {
            session.queue.push_back(Packet::Config(config.clone()));
        }
    }

    /// The generation must travel to the consumer too; stale drains are refused.
    pub fn pop(&mut self, lease: Lease) -> Result<Option<Packet>, Refusal> {
        self.check_mirror(lease)?;
        let session = self.session.as_mut().unwrap();
        let packet = session.queue.pop_front();
        if let Some(Packet::Frame(frame)) = &packet {
            session.queued_frames -= 1;
            session.queued_frame_bytes -= frame.data.len();
        }
        Ok(packet)
    }

    /// Source loss, decoder feedback or an adapter-detected dropped frame.
    pub fn discontinuity(&mut self, lease: Lease) -> Result<(), Refusal> {
        self.check_mirror(lease)?;
        Self::require_keyframe(self.session.as_mut().unwrap());
        Ok(())
    }

    pub fn mirror_audio(&self, lease: Lease) -> Result<(), Refusal> {
        self.check_mirror(lease)?;
        Err(Refusal::MirrorAudioUnsupported)
    }

    /// HID forwarding, mode commands and remote controls are intentionally absent.
    pub fn mirror_control(&self, lease: Lease) -> Result<(), Refusal> {
        self.check_mirror(lease)?;
        Err(Refusal::MirrorControlUnsupported)
    }

    pub fn status(&self) -> Status {
        let Some(session) = &self.session else {
            return Status {
                owner: None,
                phase: Phase::Idle,
                queued_frames: 0,
                queued_packets: 0,
                buffered_payload_bytes: 0,
            };
        };
        let phase = if session.lease.source == Source::CarPlay {
            Phase::CarPlayOwned
        } else if session.config.is_none() {
            Phase::AwaitingConfig
        } else if session.awaiting_keyframe {
            Phase::AwaitingKeyframe
        } else {
            Phase::Streaming
        };
        Status {
            owner: Some(session.lease),
            phase,
            queued_frames: session.queued_frames,
            queued_packets: session.queue.len(),
            buffered_payload_bytes: session.queued_frame_bytes + session.config.as_ref().map_or(0, |c| c.payload_bytes()),
        }
    }
}

fn inspect_access_unit(data: &[u8], limits: Limits) -> Result<bool, Refusal> {
    if data.len() > limits.max_frame_bytes {
        return Err(Refusal::FrameTooLarge);
    }
    if data.is_empty() {
        return Err(Refusal::EmptyFrame);
    }
    let mut offset = 0usize;
    let mut count = 0usize;
    let mut has_idr = false;
    let mut has_delta = false;
    while offset < data.len() {
        let prefix_end = offset.checked_add(4).ok_or(Refusal::InvalidNalLength)?;
        let prefix: [u8; 4] = data
            .get(offset..prefix_end)
            .ok_or(Refusal::InvalidNalLength)?
            .try_into()
            .unwrap();
        let nal_len = usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| Refusal::InvalidNalLength)?;
        let end = prefix_end
            .checked_add(nal_len)
            .ok_or(Refusal::InvalidNalLength)?;
        let nal = data
            .get(prefix_end..end)
            .filter(|nal| !nal.is_empty())
            .ok_or(Refusal::InvalidNalLength)?;
        count += 1;
        if count > limits.max_nals_per_frame {
            return Err(Refusal::TooManyNals);
        }
        if nal[0] & 0x80 != 0 {
            return Err(Refusal::InvalidNalHeader);
        }
        match nal[0] & 0x1f {
            1 => has_delta = true,
            5 if nal[0] & 0x60 != 0 => has_idr = true,
            5 => return Err(Refusal::InvalidNalHeader),
            6 | 9 if nal[0] & 0x60 == 0 => {} // SEI and AUD may precede an IDR.
            6 | 9 => return Err(Refusal::InvalidNalHeader),
            _ => return Err(Refusal::UnsupportedNalType),
        }
        offset = end;
    }
    if has_delta && has_idr {
        return Err(Refusal::MixedSliceTypes);
    }
    if !has_delta && !has_idr {
        return Err(Refusal::EmptyFrame);
    }
    Ok(has_idr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_frame_bytes: 64,
            max_parameter_set_bytes: 16,
            max_nals_per_frame: 4,
            max_queued_frames: 2,
            max_buffered_bytes: 80,
        }
    }

    fn caps() -> CarDisplayCaps {
        CarDisplayCaps {
            width: 800,
            height: 480,
            h264_profiles: &[66, 77, 100],
            max_level_idc: 31,
        }
    }

    fn config() -> ObservedConfig<'static> {
        // Synthetic NAL bodies: metadata/framing tests, not decoder fixtures.
        ObservedConfig {
            codec: Codec::H264,
            width: 800,
            height: 480,
            profile_idc: 66,
            constraint_flags: 0,
            level_idc: 31,
            rotation_degrees: 0,
            framing: Framing::Avcc4,
            sps: &[0x67, 66, 0, 31, 0x80],
            pps: &[0x68, 0x80],
        }
    }

    fn policy() -> ReceiverPolicy {
        ReceiverPolicy::new(limits(), caps(), MirroringOptIn::Enabled).unwrap()
    }

    fn ready() -> (ReceiverPolicy, Lease) {
        let mut p = policy();
        let l = p.acquire(Source::Mirroring).unwrap();
        p.configure(l, config()).unwrap();
        (p, l)
    }

    fn avcc(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| {
                (nal.len() as u32)
                    .to_be_bytes()
                    .into_iter()
                    .chain(nal.iter().copied())
            })
            .collect()
    }

    fn idr() -> Vec<u8> {
        avcc(&[&[0x65, 0x80]])
    }
    fn delta() -> Vec<u8> {
        avcc(&[&[0x41, 0x80]])
    }

    #[test]
    fn default_disabled_and_owner_conflicts_require_explicit_switch() {
        let mut disabled = ReceiverPolicy::new(limits(), caps(), MirroringOptIn::default()).unwrap();
        assert_eq!(disabled.default_source(), Source::CarPlay);
        assert_eq!(disabled.acquire(Source::Mirroring), Err(Refusal::MirroringDisabled));
        let mut p = policy();
        let car = p.acquire(Source::CarPlay).unwrap();
        assert_eq!(p.status().phase, Phase::CarPlayOwned);
        assert_eq!(p.acquire(Source::Mirroring), Err(Refusal::SourceBusy));
        assert_eq!(p.configure(car, config()), Err(Refusal::NotMirroring));
        p.release(car).unwrap();
        let mirror = p.acquire(Source::Mirroring).unwrap();
        assert!(mirror.generation() > car.generation());
        assert_eq!(p.acquire(Source::CarPlay), Err(Refusal::SourceBusy));
        p.release(mirror).unwrap();
        assert_eq!(p.status().phase, Phase::Idle);
        assert_eq!(p.status().buffered_payload_bytes, 0);
        assert!(p.acquire(p.default_source()).is_ok());
    }

    #[test]
    fn stale_owner_cannot_mutate_or_drain_new_session() {
        let (mut p, old) = ready();
        p.push_avcc_frame(old, 1, &idr()).unwrap();
        p.release(old).unwrap();
        let current = p.acquire(Source::Mirroring).unwrap();
        p.configure(current, config()).unwrap();
        p.push_avcc_frame(current, 2, &idr()).unwrap();
        let before = p.status();
        assert_eq!(p.release(old), Err(Refusal::StaleLease));
        assert_eq!(p.configure(old, config()), Err(Refusal::StaleLease));
        assert_eq!(p.push_avcc_frame(old, 3, &delta()), Err(Refusal::StaleLease));
        assert_eq!(p.discontinuity(old), Err(Refusal::StaleLease));
        assert_eq!(p.pop(old), Err(Refusal::StaleLease));
        assert_eq!(p.status(), before);
    }

    #[test]
    fn leases_from_another_policy_or_reconstruction_never_match() {
        let (mut a, lease_a) = ready();
        let (mut b, lease_b) = ready();
        assert_eq!(lease_a.generation(), lease_b.generation());
        assert_ne!(lease_a, lease_b);
        assert_eq!(b.release(lease_a), Err(Refusal::StaleLease));
        assert_eq!(b.configure(lease_a, config()), Err(Refusal::StaleLease));
        assert_eq!(b.pop(lease_a), Err(Refusal::StaleLease));
        assert_eq!(b.push_avcc_frame(lease_a, 1, &idr()), Err(Refusal::StaleLease));
        assert_eq!(b.discontinuity(lease_a), Err(Refusal::StaleLease));
        a.release(lease_a).unwrap();
        drop(a);
        let (mut rebuilt, fresh) = ready();
        assert_ne!(fresh, lease_a);
        assert_eq!(rebuilt.release(lease_a), Err(Refusal::StaleLease));
    }

    #[test]
    fn generation_exhaustion_refuses_acquire_without_wrapping() {
        let mut p = policy();
        p.generation = u64::MAX;
        assert_eq!(p.acquire(Source::Mirroring), Err(Refusal::GenerationExhausted));
        assert_eq!(p.status().phase, Phase::Idle);
    }

    #[test]
    fn config_and_actual_idr_gate_even_when_aud_or_sei_is_first() {
        let mut p = policy();
        let l = p.acquire(Source::Mirroring).unwrap();
        assert_eq!(p.push_avcc_frame(l, 1, &idr()), Err(Refusal::NeedConfig));
        p.configure(l, config()).unwrap();
        assert_eq!(p.push_avcc_frame(l, 1, &delta()), Err(Refusal::NeedKeyframe));
        let payload = avcc(&[&[0x09, 0xf0], &[0x06, 0x80], &[0x65, 0x80]]);
        p.push_avcc_frame(l, u64::MAX, &payload).unwrap();
        p.push_avcc_frame(l, 0, &delta()).unwrap();
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Config(c)) if &*c.sps == config().sps && &*c.pps == config().pps));
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Frame(f)) if f.is_idr && f.timestamp == u64::MAX && &*f.data == payload));
        // Timestamps are deliberately opaque: no monotonic/clock claim.
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Frame(f)) if !f.is_idr && f.timestamp == 0));
        assert_eq!(p.pop(l).unwrap(), None);
    }

    #[test]
    fn reconfiguration_discards_old_frames_and_demands_new_idr() {
        let (mut p, l) = ready();
        p.push_avcc_frame(l, 1, &idr()).unwrap();
        p.push_avcc_frame(l, 2, &delta()).unwrap();
        let changed = ObservedConfig {
            profile_idc: 77,
            sps: &[0x67, 77, 0, 31, 0x80],
            ..config()
        };
        p.configure(l, changed).unwrap();
        assert_eq!(p.status().queued_frames, 0);
        assert_eq!(p.status().phase, Phase::AwaitingKeyframe);
        assert_eq!(p.push_avcc_frame(l, 3, &delta()), Err(Refusal::NeedKeyframe));
        p.push_avcc_frame(l, 4, &idr()).unwrap();
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Config(c)) if c.profile_idc == 77));
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Frame(f)) if f.timestamp == 4));
    }

    #[test]
    fn rejected_config_invalidates_old_stream_and_drains_payloads() {
        let (mut p, l) = ready();
        p.push_avcc_frame(l, 1, &idr()).unwrap();
        let rejected = ObservedConfig {
            codec: Codec::Hevc,
            ..config()
        };
        assert_eq!(p.configure(l, rejected), Err(Refusal::UnsupportedCodec));
        assert_eq!(p.status().phase, Phase::AwaitingConfig);
        assert_eq!(p.status().buffered_payload_bytes, 0);
        assert_eq!(p.pop(l).unwrap(), None);
        assert_eq!(p.push_avcc_frame(l, 2, &delta()), Err(Refusal::NeedConfig));
    }

    #[test]
    fn descriptor_rejections_are_typed_and_conservative() {
        let cases = [
            (
                ObservedConfig {
                    codec: Codec::Hevc,
                    ..config()
                },
                Refusal::UnsupportedCodec,
            ),
            (
                ObservedConfig {
                    framing: Framing::AnnexB,
                    ..config()
                },
                Refusal::UnsupportedFraming,
            ),
            (
                ObservedConfig {
                    rotation_degrees: 90,
                    ..config()
                },
                Refusal::UnsupportedRotation,
            ),
            (ObservedConfig { width: 0, ..config() }, Refusal::DimensionsMismatch),
            (ObservedConfig { width: 1920, ..config() }, Refusal::DimensionsMismatch),
            (ObservedConfig { height: 800, ..config() }, Refusal::DimensionsMismatch),
            (
                ObservedConfig {
                    profile_idc: 110,
                    ..config()
                },
                Refusal::UnsupportedProfile,
            ),
            (ObservedConfig { level_idc: 42, ..config() }, Refusal::UnsupportedLevel),
            (ObservedConfig { level_idc: 0, ..config() }, Refusal::UnsupportedLevel),
            (
                ObservedConfig {
                    level_idc: 11,
                    constraint_flags: 0x10,
                    ..config()
                },
                Refusal::UnsupportedLevel,
            ),
            (
                ObservedConfig {
                    sps: &[0x67, 77, 0, 31],
                    ..config()
                },
                Refusal::InvalidParameterSets,
            ),
            (ObservedConfig { sps: &[], ..config() }, Refusal::InvalidParameterSets),
            (
                ObservedConfig {
                    pps: &[0x88, 0x80],
                    ..config()
                },
                Refusal::InvalidParameterSets,
            ),
            (
                ObservedConfig {
                    constraint_flags: 1,
                    sps: &[0x67, 66, 1, 31],
                    ..config()
                },
                Refusal::InvalidParameterSets,
            ),
            (
                ObservedConfig {
                    sps: &[0x67; 17],
                    ..config()
                },
                Refusal::ParameterSetsTooLarge,
            ),
        ];
        for (config, expected) in cases {
            let mut p = policy();
            let l = p.acquire(Source::Mirroring).unwrap();
            assert_eq!(p.configure(l, config), Err(expected));
        }
    }

    #[test]
    fn malformed_frames_and_in_band_parameter_sets_cannot_bypass_gate() {
        let cases = [
            (vec![], Refusal::EmptyFrame),
            (vec![0, 0, 0], Refusal::InvalidNalLength),
            (vec![0, 0, 0, 0], Refusal::InvalidNalLength),
            (vec![255, 255, 255, 255, 0x65], Refusal::InvalidNalLength),
            (vec![0, 0, 0, 2, 0x65], Refusal::InvalidNalLength),
            (avcc(&[&[0xe5]]), Refusal::InvalidNalHeader),
            (avcc(&[&[0x05]]), Refusal::InvalidNalHeader),
            (avcc(&[&[0x66], &[0x65]]), Refusal::InvalidNalHeader),
            (avcc(&[&[0x69], &[0x65]]), Refusal::InvalidNalHeader),
            (avcc(&[&[0x67, 66, 0, 31], &[0x65]]), Refusal::UnsupportedNalType),
            (avcc(&[&[0x68, 0x80], &[0x65]]), Refusal::UnsupportedNalType),
            (avcc(&[&[0x65], &[0x41]]), Refusal::MixedSliceTypes),
            (avcc(&[&[0x09]]), Refusal::EmptyFrame),
            (avcc(&[&[0x09], &[0x09], &[0x09], &[0x09], &[0x65]]), Refusal::TooManyNals),
            (vec![0; 65], Refusal::FrameTooLarge),
        ];
        for (payload, expected) in cases {
            let (mut p, l) = ready();
            p.push_avcc_frame(l, 1, &idr()).unwrap();
            assert_eq!(p.push_avcc_frame(l, 2, &payload), Err(expected));
            assert_eq!(p.status().queued_frames, 0);
            assert_eq!(p.status().phase, Phase::AwaitingKeyframe);
            assert_eq!(p.push_avcc_frame(l, 3, &delta()), Err(Refusal::NeedKeyframe));
        }
    }

    #[test]
    fn frame_count_overflow_flushes_dependencies_and_repeats_config() {
        let (mut p, l) = ready();
        p.push_avcc_frame(l, 1, &idr()).unwrap();
        p.push_avcc_frame(l, 2, &delta()).unwrap();
        assert_eq!(p.push_avcc_frame(l, 3, &delta()), Err(Refusal::QueueFull));
        assert_eq!(p.status().queued_packets, 1);
        assert_eq!(p.status().queued_frames, 0);
        assert_eq!(p.status().buffered_payload_bytes, 7);
        assert_eq!(p.push_avcc_frame(l, 4, &delta()), Err(Refusal::NeedKeyframe));
        p.push_avcc_frame(l, 5, &idr()).unwrap();
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Config(_))));
        assert!(matches!(p.pop(l).unwrap(), Some(Packet::Frame(f)) if f.timestamp == 5));
    }

    #[test]
    fn byte_budget_counts_retained_parameter_sets_and_releases_frames_on_pop() {
        let mut p = ReceiverPolicy::new(
            Limits {
                max_queued_frames: 10,
                ..limits()
            },
            caps(),
            MirroringOptIn::Enabled,
        )
        .unwrap();
        let l = p.acquire(Source::Mirroring).unwrap();
        p.configure(l, config()).unwrap();
        let big_idr = avcc(&[&[0x65; 36]]);
        p.push_avcc_frame(l, 1, &big_idr).unwrap();
        assert_eq!(p.status().buffered_payload_bytes, 47);
        assert_eq!(p.push_avcc_frame(l, 2, &big_idr), Err(Refusal::QueueFull));
        assert_eq!(p.status().buffered_payload_bytes, 7);
        p.push_avcc_frame(l, 3, &big_idr).unwrap();
        p.pop(l).unwrap();
        assert_eq!(p.status().buffered_payload_bytes, 47);
        p.pop(l).unwrap();
        assert_eq!(p.status().buffered_payload_bytes, 7);
        p.release(l).unwrap();
        assert_eq!(p.status().buffered_payload_bytes, 0);
    }

    #[test]
    fn shared_config_payload_is_not_duplicated_in_the_queue() {
        let (mut p, l) = ready();
        let retained = p.session.as_ref().unwrap().config.as_ref().unwrap().clone();
        let Some(Packet::Config(queued)) = p.pop(l).unwrap() else {
            panic!("config missing")
        };
        assert!(Arc::ptr_eq(&retained, &queued));
        assert_eq!(p.status().buffered_payload_bytes, 7);
    }

    #[test]
    fn discontinuity_audio_control_and_teardown_contract() {
        let (mut p, l) = ready();
        p.push_avcc_frame(l, 1, &idr()).unwrap();
        p.discontinuity(l).unwrap();
        assert_eq!(p.status().phase, Phase::AwaitingKeyframe);
        assert_eq!(p.mirror_audio(l), Err(Refusal::MirrorAudioUnsupported));
        assert_eq!(p.mirror_control(l), Err(Refusal::MirrorControlUnsupported));
        p.release(l).unwrap();
        assert_eq!(p.status().queued_packets, 0);
        assert_eq!(p.status().buffered_payload_bytes, 0);
        assert_eq!(p.pop(l), Err(Refusal::StaleLease));
    }

    #[test]
    fn invalid_limits_and_receiver_caps_are_rejected() {
        assert!(matches!(
            ReceiverPolicy::new(
                Limits {
                    max_frame_bytes: usize::MAX,
                    ..limits()
                },
                caps(),
                MirroringOptIn::Enabled
            ),
            Err(Refusal::InvalidLimits)
        ));
        assert!(matches!(
            ReceiverPolicy::new(
                Limits {
                    max_queued_frames: 0,
                    ..limits()
                },
                caps(),
                MirroringOptIn::Enabled
            ),
            Err(Refusal::InvalidLimits)
        ));
        assert!(matches!(
            ReceiverPolicy::new(limits(), CarDisplayCaps { width: 0, ..caps() }, MirroringOptIn::Enabled),
            Err(Refusal::InvalidCaps)
        ));
        assert!(matches!(
            ReceiverPolicy::new(
                limits(),
                CarDisplayCaps {
                    h264_profiles: &[110],
                    ..caps()
                },
                MirroringOptIn::Enabled
            ),
            Err(Refusal::InvalidCaps)
        ));
    }
}

#[cfg(test)]
mod malformed_input_regression {
    use super::*;

    #[test]
    fn bounded_arbitrary_input_never_panics_or_exceeds_payload_budget() {
        let limits = Limits {
            max_frame_bytes: 128,
            max_parameter_set_bytes: 16,
            max_nals_per_frame: 8,
            max_queued_frames: 3,
            max_buffered_bytes: 256,
        };
        let caps = CarDisplayCaps {
            width: 800,
            height: 480,
            h264_profiles: &[66],
            max_level_idc: 31,
        };
        let mut policy = ReceiverPolicy::new(limits, caps, MirroringOptIn::Enabled).unwrap();
        let lease = policy.acquire(Source::Mirroring).unwrap();
        policy
            .configure(
                lease,
                ObservedConfig {
                    codec: Codec::H264,
                    width: 800,
                    height: 480,
                    profile_idc: 66,
                    constraint_flags: 0,
                    level_idc: 31,
                    rotation_degrees: 0,
                    framing: Framing::Avcc4,
                    sps: &[0x67, 66, 0, 31, 0x80],
                    pps: &[0x68, 0x80],
                },
            )
            .unwrap();
        let mut seed = 0x1234_5678u32;
        for len in 0..=256 {
            for _ in 0..16 {
                let input: Vec<u8> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        seed as u8
                    })
                    .collect();
                let _ = policy.push_avcc_frame(lease, u64::from(seed), &input);
                let status = policy.status();
                assert!(status.buffered_payload_bytes <= limits.max_buffered_bytes);
                assert!(status.queued_frames <= limits.max_queued_frames);
            }
        }
    }
}
