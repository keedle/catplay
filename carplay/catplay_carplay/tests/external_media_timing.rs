use std::time::{Duration, Instant};

use bytes::BytesMut;
use catplay_carplay::{
    clock::{MediaClock, NtpU64},
    screen::{ScreenFlag, ScreenFrame},
    video::{
        AvccConfig, AvccConfigExtended, CompressedAccessUnit, ExternalMediaTiming,
        ExternalMediaTimingError, MediaTimeEpoch, EXTERNAL_MEDIA_MAX_BYTES,
        EXTERNAL_MEDIA_MAX_FRAMES,
    },
};

fn config() -> AvccConfigExtended {
    AvccConfigExtended {
        hevc: false,
        avcc: AvccConfig {
            nal_size_len: 4,
            sps_pps: include_bytes!("fixtures/external_media/main31-sps-pps.annexb").to_vec(),
        },
        video_latency: Duration::from_millis(40),
        width: 960,
        height: 540,
        respect_timestamps: true,
    }
}

fn unit(dts_ns: i64, pts_ns: i64, is_idr: bool, size: usize) -> CompressedAccessUnit {
    CompressedAccessUnit {
        dts_ns,
        pts_ns,
        is_idr,
        payload: BytesMut::from(vec![0x65; size].as_slice()),
    }
}

fn ready() -> (ExternalMediaTiming, u64) {
    let mut timing = ExternalMediaTiming::new(Duration::from_millis(100)).unwrap();
    let generation = timing.generation();
    timing.install_config(generation, config()).unwrap();
    (timing, generation)
}

#[test]
fn keeps_decode_order_and_maps_regressing_pts_separately_from_dts() {
    let origin = Instant::now();
    let origin = origin + Duration::from_millis(200);
    let epoch = MediaTimeEpoch::new(0, origin);
    let shared_epoch = epoch;
    assert_eq!(epoch.map(12_000_000).unwrap(), shared_epoch.map(12_000_000).unwrap());
    let (mut timing, generation) = ready();
    timing.push(generation, unit(-30_000_000, 36_000_000, true, 8)).unwrap();
    timing.push(generation, unit(0, 66_000_000, false, 8)).unwrap();
    timing.push(generation, unit(33_000_000, 60_000_000, false, 8)).unwrap();

    let now = origin - Duration::from_millis(50);
    assert!(timing.pop_due(origin - Duration::from_millis(140), &epoch).unwrap().is_none());
    let first = timing.pop_due(now, &epoch).unwrap().unwrap();
    assert_eq!(first.release_at, origin - Duration::from_millis(130));
    let first_frame = first.into_encoded_video_frame(&epoch).unwrap();
    assert_eq!(first_frame.pts.0, origin + Duration::from_millis(36));
    assert!(first_frame.pts.0 > now);
    assert!(first_frame.config.is_some());

    let second = timing.pop_due(now, &epoch).unwrap().unwrap();
    assert_eq!(second.release_at, origin - Duration::from_millis(100));
    let second_frame = second.into_encoded_video_frame(&epoch).unwrap();
    assert_eq!(second_frame.pts.0, origin + Duration::from_millis(66));
    assert!(second_frame.pts.0 > now);
    assert!(second_frame.config.is_none());

    let third = timing.pop_due(now, &epoch).unwrap().unwrap();
    assert_eq!(third.release_at, origin - Duration::from_millis(67));
    let third_frame = third.into_encoded_video_frame(&epoch).unwrap();
    assert_eq!(third_frame.pts.0, origin + Duration::from_millis(60));
    assert!(third_frame.pts.0 > now && third_frame.pts.0 < second_frame.pts.0);
    assert!(timing.pop_due(now, &epoch).unwrap().is_none());
}

#[test]
fn accepts_signed_initial_dts_rejects_nonmonotonic_dts_and_enforces_caps() {
    let origin = Instant::now();
    let epoch = MediaTimeEpoch::new(0, origin);
    let (mut timing, generation) = ready();
    timing.push(generation, unit(-10, 0, true, 1)).unwrap();
    assert_eq!(timing.push(generation, unit(-10, -5, false, 1)), Err(ExternalMediaTimingError::NonMonotonicDts));
    assert_eq!(timing.push(generation, unit(-11, -5, false, 1)), Err(ExternalMediaTimingError::NonMonotonicDts));
    assert_eq!(
        ExternalMediaTiming::new(Duration::from_millis(501)).unwrap_err(),
        ExternalMediaTimingError::DecodeLeadTooLarge,
    );

    let (mut frames, generation) = ready();
    for index in 0..EXTERNAL_MEDIA_MAX_FRAMES {
        frames.push(generation, unit(index as i64, 0, index == 0, 1)).unwrap();
    }
    assert_eq!(frames.queued_frames(), EXTERNAL_MEDIA_MAX_FRAMES);
    assert_eq!(frames.push(generation, unit(9, 0, false, 1)), Err(ExternalMediaTimingError::FrameLimit));

    let (mut bytes, generation) = ready();
    let mut overallocated = BytesMut::with_capacity(EXTERNAL_MEDIA_MAX_BYTES + 1);
    overallocated.extend_from_slice(&[0x65]);
    let retained_capacity = overallocated.capacity();
    assert!(retained_capacity > EXTERNAL_MEDIA_MAX_BYTES);
    let mut oversized = unit(0, 0, true, 0);
    oversized.payload = overallocated;
    assert_eq!(bytes.push(generation, oversized), Err(ExternalMediaTimingError::ByteLimit));
    let mut retained = BytesMut::with_capacity(1024);
    retained.extend_from_slice(&[0x65]);
    let retained_capacity = retained.capacity();
    let mut small = unit(0, 0, true, 0);
    small.payload = retained;
    bytes.push(generation, small).unwrap();
    assert_eq!(bytes.queued_bytes(), retained_capacity);
}

#[test]
fn seek_flush_requires_current_generation_config_and_idr_then_maps_screen_headers() {
    let origin = Instant::now();
    let epoch = MediaTimeEpoch::new(0, origin);
    let mut timing = ExternalMediaTiming::new(Duration::from_millis(100)).unwrap();
    let old_generation = timing.generation();
    timing.install_config(old_generation, config()).unwrap();
    timing.push(old_generation, unit(0, 0, true, 8)).unwrap();
    timing.push(old_generation, unit(33_000_000, 33_000_000, false, 8)).unwrap();

    let generation = timing.flush_seek().unwrap();
    assert_eq!(generation, old_generation + 1);
    assert_eq!(timing.queued_frames(), 0);
    assert_eq!(timing.queued_bytes(), 0);
    assert_eq!(timing.push(old_generation, unit(66_000_000, 66_000_000, true, 8)), Err(ExternalMediaTimingError::StaleGeneration));
    assert_eq!(timing.push(generation, unit(66_000_000, 66_000_000, true, 8)), Err(ExternalMediaTimingError::MissingConfig));
    timing.install_config(generation, config()).unwrap();
    assert_eq!(timing.push(generation, unit(66_000_000, 66_000_000, false, 8)), Err(ExternalMediaTimingError::NeedIdr));
    let mut restart = unit(300_000_000, 400_000_000, true, 0);
    let synthetic_idr = include_bytes!("fixtures/external_media/main31-idr.annexb");
    restart.payload = BytesMut::from(synthetic_idr.as_slice());
    timing.push(generation, restart).unwrap();

    let output_config = ScreenFrame::config(&config());
    assert!(output_config.flags().contains(ScreenFlag::RespectTimestamps));
    let decoded_config = output_config.config_decode(Duration::from_millis(40)).unwrap();
    assert!(decoded_config.respect_timestamps);
    assert_eq!(decoded_config.avcc.sps_pps, config().avcc.sps_pps);
    assert_eq!(decoded_config.avcc.sps_pps[5], 77);
    assert_eq!(decoded_config.avcc.sps_pps[7], 31);

    let released = timing.pop_due(origin + Duration::from_millis(200), &epoch).unwrap().unwrap();
    assert_eq!(released.generation, generation);
    assert_eq!(released.release_at, origin + Duration::from_millis(200));
    let encoded = released.into_encoded_video_frame(&epoch).unwrap();
    let expected_pts = origin + Duration::from_millis(400);
    assert!(expected_pts > origin + Duration::from_millis(200));
    assert_eq!(encoded.pts.0, expected_pts);

    let clock = TestClock { epoch: origin };
    let outbound = ScreenFrame::video_proxied(encoded, 4, &clock).unwrap();
    let expected_nal = &synthetic_idr[4..];
    assert_eq!(&outbound.data[..4], &(expected_nal.len() as u32).to_be_bytes());
    assert_eq!(&outbound.data[4..], expected_nal);
    assert_eq!(outbound.header.params[0].as_u64(), clock.encode_local(expected_pts).0);
}

struct TestClock { epoch: Instant }

impl MediaClock for TestClock {
    fn is_synchronized(&self) -> bool { true }
    fn decode_remote(&self, pts: NtpU64) -> Option<Instant> { Some(self.decode_local(pts)) }
    fn decode_local(&self, pts: NtpU64) -> Instant {
        self.epoch + Duration::from_nanos(pts.as_nanos() as u64)
    }
    fn encode_local(&self, pts: Instant) -> NtpU64 {
        NtpU64::from_monotonic_nanos(pts.duration_since(self.epoch).as_nanos() as i128)
    }
    fn encode_remote(&self, pts: Instant) -> Option<NtpU64> { Some(self.encode_local(pts)) }
}
