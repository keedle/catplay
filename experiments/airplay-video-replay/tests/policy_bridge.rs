//! Combines the new policy with the actual upstream framing code.
//! The descriptor and tiny NALs are synthetic, not a measured Peugeot capability
//! or a decodable iPhone picture. This does not exercise a network session.
use airplay_mirroring_experiment::*;
use bytes::BytesMut;
use catplay_airplay_video_replay::video::*;

const SPS: &[u8] = &[0x67, 66, 224, 30, 0x89, 0x8b];
const PPS: &[u8] = &[0x68, 0xce];

fn policy() -> ReceiverPolicy {
    ReceiverPolicy::new(
        Limits {
            max_frame_bytes: 128,
            max_parameter_set_bytes: 256,
            max_nals_per_frame: 8,
            max_queued_frames: 4,
            max_buffered_bytes: 1024,
        },
        CarDisplayCaps {
            width: 800,
            height: 480,
            h264_profiles: &[66],
            max_level_idc: 31,
        },
        MirroringOptIn::Enabled,
    )
    .unwrap()
}

fn config() -> ObservedConfig<'static> {
    ObservedConfig {
        codec: Codec::H264,
        width: 800,
        height: 480,
        profile_idc: 66,
        constraint_flags: 224,
        level_idc: 30,
        rotation_degrees: 0,
        framing: Framing::Avcc4,
        sps: SPS,
        pps: PPS,
    }
}

#[test]
fn accepted_config_idr_and_delta_survive_actual_transmitter_framing() {
    let mut policy = policy();
    let lease = policy.acquire(Source::Mirroring).unwrap();
    policy.configure(lease.clone(), config()).unwrap();
    // AUD + SEI + IDR tests IDR detection after leading non-picture NALs.
    let idr = [0, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 3, 0x06, 0x05, 0x03, 0, 0, 0, 3, 0x65, 0x88, 0x84];
    let delta = [0, 0, 0, 3, 0x41, 0x9a, 0x20];
    policy
        .push_avcc_frame(lease.clone(), 1_000_000, &idr)
        .unwrap();
    policy
        .push_avcc_frame(lease.clone(), 34_333_333, &delta)
        .unwrap();
    let Some(Packet::Config(accepted)) = policy.pop(lease.clone()).unwrap() else {
        panic!("config must precede picture");
    };
    let mut sets = vec![0, 0, 0, 1];
    sets.extend_from_slice(&accepted.sps);
    sets.extend_from_slice(&[0, 0, 0, 1]);
    sets.extend_from_slice(&accepted.pps);
    let wire_config = avcc_config_serialize(&AvccConfig {
        nal_size_len: 4,
        sps_pps: sets.clone(),
    })
    .unwrap();
    assert_eq!(avcc_config_deserialize(&wire_config).unwrap().sps_pps, sets);
    for (expected, pts, keyframe) in [(&idr[..], 1_000_000, true), (&delta[..], 34_333_333, false)] {
        let Some(Packet::Frame(frame)) = policy.pop(lease.clone()).unwrap() else {
            panic!("picture expected");
        };
        assert_eq!(frame.timestamp, pts);
        assert_eq!(frame.is_idr, keyframe);
        let mut avcc = BytesMut::from(&*frame.data);
        let mut rx = AnnexBConverter::new(4);
        let mut annexb = rx.convert(&mut avcc, false).unwrap();
        let mut tx = AnnexBConverter::with_cache(4, rx.offsets_cache);
        assert_eq!(&tx.convert(&mut annexb, true).unwrap()[..], expected);
    }
    assert!(policy.pop(lease.clone()).unwrap().is_none());
    policy.release(lease).unwrap();
    assert_eq!(policy.status().buffered_payload_bytes, 0);
}

#[test]
fn rejected_second_connection_and_old_drop_leave_new_owner_intact() {
    let mut policy = policy();
    let car = policy.acquire(Source::CarPlay).unwrap();
    assert_eq!(policy.acquire(Source::Mirroring), Err(Refusal::SourceBusy));
    assert_eq!(policy.status().owner, Some(car.clone()));
    policy.release(car.clone()).unwrap();
    let mirror = policy.acquire(Source::Mirroring).unwrap();
    policy.configure(mirror.clone(), config()).unwrap();
    assert_eq!(policy.release(car), Err(Refusal::StaleLease));
    assert_eq!(policy.status().owner, Some(mirror.clone()));
    assert!(matches!(policy.pop(mirror).unwrap(), Some(Packet::Config(_))));
}

#[test]
fn stale_configuration_cannot_reconfigure_reconnected_mirror() {
    let mut policy = policy();
    let old = policy.acquire(Source::Mirroring).unwrap();
    policy.release(old.clone()).unwrap();
    let current = policy.acquire(Source::Mirroring).unwrap();
    policy.configure(current.clone(), config()).unwrap();
    let before = policy.status();
    let mut incompatible = config();
    incompatible.codec = Codec::Hevc;
    assert_eq!(policy.configure(old, incompatible), Err(Refusal::StaleLease));
    assert_eq!(policy.status(), before);
}
