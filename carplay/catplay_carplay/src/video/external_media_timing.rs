//! Bounded decode-order queue and shared media-to-monotonic timestamp epoch
//! for externally demuxed compressed video.

use std::{collections::VecDeque, time::{Duration, Instant}};

use bytes::BytesMut;
use thiserror::Error;

use crate::video::{AvccConfigExtended, EncodedVideoFrame, Pts};

pub const EXTERNAL_MEDIA_MAX_FRAMES: usize = 8;
pub const EXTERNAL_MEDIA_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const EXTERNAL_MEDIA_DEFAULT_DECODE_LEAD: Duration = Duration::from_millis(100);
pub const EXTERNAL_MEDIA_MAX_DECODE_LEAD: Duration = Duration::from_millis(500);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExternalMediaTimingError {
    #[error("access unit belongs to a stale generation")]
    StaleGeneration,
    #[error("decode timestamps must increase in decode order")]
    NonMonotonicDts,
    #[error("presentation timestamp cannot be mapped to the monotonic clock")]
    TimestampOutOfRange,
    #[error("external media queue is full")]
    FrameLimit,
    #[error("external media queue byte limit exceeded")]
    ByteLimit,
    #[error("video configuration is required before access units")]
    MissingConfig,
    #[error("the first access unit after startup or seek must be an IDR")]
    NeedIdr,
    #[error("configuration cannot change while access units are queued")]
    ConfigWhileQueued,
    #[error("media generation counter exhausted")]
    GenerationExhausted,
    #[error("decode lead exceeds the configured maximum")]
    DecodeLeadTooLarge,
}

/// Maps a media timestamp domain to one monotonic clock epoch shared by every
/// track in a media presentation.
#[derive(Debug, Clone, Copy)]
pub struct MediaTimeEpoch {
    media_origin_ns: i64,
    instant_origin: Instant,
}

impl MediaTimeEpoch {
    pub fn new(media_origin_ns: i64, instant_origin: Instant) -> Self {
        Self { media_origin_ns, instant_origin }
    }

    pub fn map(&self, media_time_ns: i64) -> Result<Instant, ExternalMediaTimingError> {
        let delta = media_time_ns as i128 - self.media_origin_ns as i128;
        let magnitude = delta.unsigned_abs();
        if magnitude > u64::MAX as u128 {
            return Err(ExternalMediaTimingError::TimestampOutOfRange);
        }
        let duration = Duration::from_nanos(magnitude as u64);
        if delta >= 0 {
            self.instant_origin.checked_add(duration)
        } else {
            self.instant_origin.checked_sub(duration)
        }
        .ok_or(ExternalMediaTimingError::TimestampOutOfRange)
    }
}

/// One compressed access unit in demuxer decode order. PTS may regress as B
/// frames are presented in a different order from their DTS values.
#[derive(Debug, Clone)]
pub struct CompressedAccessUnit {
    pub dts_ns: i64,
    pub pts_ns: i64,
    pub is_idr: bool,
    pub payload: BytesMut,
}

#[derive(Debug, Clone)]
struct QueuedUnit {
    generation: u64,
    unit: CompressedAccessUnit,
}

/// An access unit released when its DTS is due. DTS is represented only as a
/// pacing deadline; presentation time is mapped separately when constructing
/// the video frame.
#[derive(Debug, Clone)]
pub struct ReleasedAccessUnit {
    pub generation: u64,
    pub release_at: Instant,
    unit: CompressedAccessUnit,
    width: u32,
    height: u32,
    config: Option<AvccConfigExtended>,
}

impl ReleasedAccessUnit {
    pub fn into_encoded_video_frame(
        self,
        epoch: &MediaTimeEpoch,
    ) -> Result<EncodedVideoFrame, ExternalMediaTimingError> {
        let pts = epoch.map(self.unit.pts_ns)?;
        Ok(EncodedVideoFrame {
            pts: Pts::from(pts),
            width: self.width,
            height: self.height,
            data: self.unit.payload,
            config: self.config,
            nal_offsets: None,
            is_keyframe: Some(self.unit.is_idr),
            header_buf: BytesMut::new(),
            chacha_tag_buf: BytesMut::new(),
        })
    }
}

/// A bounded FIFO for compressed video access units. It preserves demux
/// decode order and never sorts by PTS. The 8-frame and 2 MiB limits cover
/// queued frame count and retained payload capacity; they do not bound total
/// process memory.
#[derive(Debug)]
pub struct ExternalMediaTiming {
    generation: u64,
    queue: VecDeque<QueuedUnit>,
    queued_bytes: usize,
    last_dts_ns: Option<i64>,
    config: Option<AvccConfigExtended>,
    needs_idr: bool,
    decode_lead: Duration,
}

impl ExternalMediaTiming {
    pub fn new(decode_lead: Duration) -> Result<Self, ExternalMediaTimingError> {
        if decode_lead > EXTERNAL_MEDIA_MAX_DECODE_LEAD {
            return Err(ExternalMediaTimingError::DecodeLeadTooLarge);
        }
        Ok(Self { decode_lead, ..Self::default() })
    }

    pub fn generation(&self) -> u64 { self.generation }
    pub fn queued_frames(&self) -> usize { self.queue.len() }
    pub fn queued_bytes(&self) -> usize { self.queued_bytes }

    pub fn install_config(
        &mut self,
        generation: u64,
        config: AvccConfigExtended,
    ) -> Result<(), ExternalMediaTimingError> {
        if generation != self.generation { return Err(ExternalMediaTimingError::StaleGeneration); }
        if !self.queue.is_empty() { return Err(ExternalMediaTimingError::ConfigWhileQueued); }
        self.config = Some(config);
        self.needs_idr = true;
        Ok(())
    }

    pub fn push(
        &mut self,
        generation: u64,
        unit: CompressedAccessUnit,
    ) -> Result<(), ExternalMediaTimingError> {
        if generation != self.generation { return Err(ExternalMediaTimingError::StaleGeneration); }
        if self.last_dts_ns.is_some_and(|last| unit.dts_ns <= last) {
            return Err(ExternalMediaTimingError::NonMonotonicDts);
        }
        if self.config.is_none() { return Err(ExternalMediaTimingError::MissingConfig); }
        if self.needs_idr && !unit.is_idr { return Err(ExternalMediaTimingError::NeedIdr); }
        if self.queue.len() >= EXTERNAL_MEDIA_MAX_FRAMES {
            return Err(ExternalMediaTimingError::FrameLimit);
        }
        let new_size = self.queued_bytes.checked_add(unit.payload.capacity())
            .ok_or(ExternalMediaTimingError::ByteLimit)?;
        if new_size > EXTERNAL_MEDIA_MAX_BYTES { return Err(ExternalMediaTimingError::ByteLimit); }
        self.last_dts_ns = Some(unit.dts_ns);
        self.queued_bytes = new_size;
        self.needs_idr = false;
        self.queue.push_back(QueuedUnit { generation, unit });
        Ok(())
    }

    pub fn pop_due(
        &mut self,
        now: Instant,
        epoch: &MediaTimeEpoch,
    ) -> Result<Option<ReleasedAccessUnit>, ExternalMediaTimingError> {
        let Some(front) = self.queue.front() else { return Ok(None); };
        let release_at = epoch.map(front.unit.dts_ns)?
            .checked_sub(self.decode_lead)
            .ok_or(ExternalMediaTimingError::TimestampOutOfRange)?;
        if release_at > now { return Ok(None); }
        let queued = self.queue.pop_front().expect("front was present");
        self.queued_bytes -= queued.unit.payload.capacity();
        let config = self.config.as_ref().ok_or(ExternalMediaTimingError::MissingConfig)?;
        let width = config.width;
        let height = config.height;
        let frame_config = queued.unit.is_idr.then(|| config.clone());
        Ok(Some(ReleasedAccessUnit {
            generation: queued.generation,
            release_at,
            unit: queued.unit,
            width,
            height,
            config: frame_config,
        }))
    }

    /// Flushes queued media and starts a new seek generation. New config and
    /// an IDR access unit are required before frames can be released.
    pub fn flush_seek(&mut self) -> Result<u64, ExternalMediaTimingError> {
        self.generation = self.generation.checked_add(1)
            .ok_or(ExternalMediaTimingError::GenerationExhausted)?;
        self.queue.clear();
        self.queued_bytes = 0;
        self.last_dts_ns = None;
        self.config = None;
        self.needs_idr = true;
        Ok(self.generation)
    }
}

impl Default for ExternalMediaTiming {
    fn default() -> Self {
        Self {
            generation: 0,
            queue: VecDeque::new(),
            queued_bytes: 0,
            last_dts_ns: None,
            config: None,
            needs_idr: true,
            decode_lead: EXTERNAL_MEDIA_DEFAULT_DECODE_LEAD,
        }
    }
}
