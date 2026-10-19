use airplay_mirroring_experiment::{
    CarDisplayCaps, Codec, Framing, Limits, MirroringOptIn, ObservedConfig, Packet, ReceiverPolicy, Refusal, Source,
};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] || args.is_empty() {
        println!(
            "Usage: mirror-replay --enable-mirroring\nHost-only synthetic framing/ownership replay; no network, receiver or video decoding."
        );
        return;
    }
    if args != ["--enable-mirroring"] {
        eprintln!("Expected --enable-mirroring, or --help.");
        std::process::exit(2);
    }
    if let Err(error) = replay() {
        eprintln!("Replay failed: {error:?}");
        std::process::exit(1);
    }
}

fn replay() -> Result<(), Refusal> {
    let limits = Limits {
        max_frame_bytes: 1024,
        max_parameter_set_bytes: 128,
        max_nals_per_frame: 16,
        max_queued_frames: 2,
        max_buffered_bytes: 2176,
    };
    let caps = CarDisplayCaps {
        width: 800,
        height: 480,
        h264_profiles: &[66],
        max_level_idc: 31,
    };
    let mut disabled = ReceiverPolicy::new(limits, caps, MirroringOptIn::default())?;
    assert_eq!(disabled.acquire(Source::Mirroring), Err(Refusal::MirroringDisabled));
    println!("PASS: mirroring is disabled by default.");

    let mut policy = ReceiverPolicy::new(limits, caps, MirroringOptIn::Enabled)?;
    let carplay = policy.acquire(Source::CarPlay)?;
    assert_eq!(policy.acquire(Source::Mirroring), Err(Refusal::SourceBusy));
    policy.release(carplay)?;
    let mirror = policy.acquire(Source::Mirroring)?;
    assert_eq!(policy.release(carplay), Err(Refusal::StaleLease));
    println!("PASS: explicit source release/acquire, stale teardown refused.");

    // Deliberately synthetic NAL bodies. This replay proves framing/policy only,
    // not decodable SPS/PPS/slice syntax or head-unit decoder compatibility.
    let config = ObservedConfig {
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
    };
    policy.configure(mirror, config)?;
    let idr = [0, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 2, 0x65, 0x80];
    let delta = [0, 0, 0, 2, 0x41, 0x80];
    assert_eq!(policy.push_avcc_frame(mirror, 9, &delta), Err(Refusal::NeedKeyframe));
    policy.push_avcc_frame(mirror, 10, &idr)?;
    policy.push_avcc_frame(mirror, 11, &delta)?;
    assert!(matches!(policy.pop(mirror)?, Some(Packet::Config(_))));
    assert!(matches!(policy.pop(mirror)?, Some(Packet::Frame(frame)) if frame.is_idr && frame.timestamp == 10 && &*frame.data == idr));
    assert!(matches!(policy.pop(mirror)?, Some(Packet::Frame(frame)) if !frame.is_idr && frame.timestamp == 11 && &*frame.data == delta));
    println!("PASS: config precedes AUD+IDR, then delta; compressed bytes and opaque timestamps preserved.");
    assert_eq!(policy.mirror_audio(mirror), Err(Refusal::MirrorAudioUnsupported));
    assert_eq!(policy.mirror_control(mirror), Err(Refusal::MirrorControlUnsupported));
    policy.release(mirror)?;
    assert_eq!(policy.status().buffered_payload_bytes, 0);
    let restored = policy.acquire(policy.default_source())?;
    policy.release(restored)?;
    println!("PASS: audio/control refused; teardown drains payloads and makes CarPlay selection available.");
    println!("Synthetic replay passed. No real AirPlay session, decoding, vehicle or target performance was tested.");
    Ok(())
}
