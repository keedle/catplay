//! Synthetic media packets through the real AES RX, compressed frame and ChaCha
//! TX code. This does not perform pairing, network clock sync or video decoding.
use bytes::BytesMut;
use catplay_carplay::{
    clock::{MediaClock, NtpU64},
    screen::{ScreenFrame, ScreenFrameCodec},
    video::{AvccConfig, AvccConfigExtended},
};
use catplay_tokio::{Decoder, Encoder};
use std::time::{Duration, Instant};

struct ControlledClock {
    epoch: Instant,
    offset_ns: i128,
}

impl MediaClock for ControlledClock {
    fn is_synchronized(&self) -> bool {
        true
    }
    fn decode_remote(&self, pts: NtpU64) -> Option<Instant> {
        Some(self.decode_local(pts))
    }
    fn decode_local(&self, pts: NtpU64) -> Instant {
        self.epoch + Duration::from_nanos((pts.as_nanos() - self.offset_ns) as u64)
    }
    fn encode_local(&self, pts: Instant) -> NtpU64 {
        NtpU64::from_monotonic_nanos(self.offset_ns + pts.duration_since(self.epoch).as_nanos() as i128)
    }
    fn encode_remote(&self, pts: Instant) -> Option<NtpU64> {
        Some(self.encode_local(pts))
    }
}

fn fragmented(codec: &mut ScreenFrameCodec, wire: &[u8]) -> ScreenFrame {
    let mut input = BytesMut::new();
    let mut result = None;
    // Splits headers, payload and authentication tags at arbitrary boundaries.
    for chunk in wire.chunks(7) {
        input.extend_from_slice(chunk);
        if let Some(frame) = codec.decode(&mut input).unwrap() {
            assert!(result.replace(frame).is_none());
        }
    }
    assert!(input.is_empty());
    result.expect("complete media packet")
}

#[test]
fn aes_to_chacha_preserves_config_pictures_and_reencodes_destination_time() {
    let epoch = Instant::now();
    let phone_clock = ControlledClock {
        epoch,
        offset_ns: 1_000_000_000,
    };
    let car_clock = ControlledClock {
        epoch,
        offset_ns: 20_000_000_000,
    };
    let mut phone = ScreenFrameCodec::aes([0x11; 16], 101, false);
    let mut mirror_rx = ScreenFrameCodec::aes([0x11; 16], 101, true);
    let mut car_tx = ScreenFrameCodec::chacha([0x22; 32], 202, false);
    let mut car_rx = ScreenFrameCodec::chacha([0x22; 32], 202, true);
    let config = AvccConfigExtended {
        hevc: false,
        avcc: AvccConfig {
            nal_size_len: 4,
            sps_pps: vec![0, 0, 0, 1, 0x67, 66, 224, 30, 0x89, 0x8b, 0, 0, 0, 1, 0x68, 0xce],
        },
        video_latency: Duration::from_millis(40),
        width: 800,
        height: 480,
        respect_timestamps: true,
    };
    let mut wire = BytesMut::new();
    phone
        .encode(ScreenFrame::config(&config), &mut wire)
        .unwrap();
    let received = fragmented(&mut mirror_rx, &wire)
        .config_decode(config.video_latency)
        .unwrap();
    assert_eq!(received, config);
    wire.clear();
    car_tx
        .encode(ScreenFrame::config(&received), &mut wire)
        .unwrap();
    assert_eq!(
        fragmented(&mut car_rx, &wire)
            .config_decode(config.video_latency)
            .unwrap(),
        config
    );

    let pictures: [(&[u8], bool); 2] = [
        (
            &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x06, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84],
            true,
        ),
        (&[0, 0, 0, 1, 0x41, 0x9a, 0x20], false),
    ];
    for (index, (payload, is_idr)) in pictures.into_iter().enumerate() {
        let pts = epoch + Duration::from_millis(100 + 33 * index as u64);
        let packet = ScreenFrame::video_annexb(BytesMut::from(payload), 4, pts, &phone_clock).unwrap();
        let source_time = packet.header.params[0].as_u64();
        wire.clear();
        phone.encode(packet, &mut wire).unwrap();
        let encoded = fragmented(&mut mirror_rx, &wire)
            .video_decode(&config, &phone_clock)
            .unwrap()
            .unwrap();
        assert_eq!(&encoded.data[..], payload);
        assert_eq!(encoded.is_keyframe, Some(is_idr));
        let expected_destination_time = car_clock.encode_local(encoded.pts.0).0;
        let outbound = ScreenFrame::video_proxied(encoded, 4, &car_clock).unwrap();
        assert_eq!(outbound.header.params[0].as_u64(), expected_destination_time);
        assert_ne!(outbound.header.params[0].as_u64(), source_time);
        wire.clear();
        car_tx.encode(outbound, &mut wire).unwrap();
        let delivered = fragmented(&mut car_rx, &wire)
            .video_decode(&config, &car_clock)
            .unwrap()
            .unwrap();
        assert_eq!(&delivered.data[..], payload);
        assert_eq!(delivered.config, Some(config.clone()));
        assert_eq!(delivered.is_keyframe, Some(is_idr));
        assert!(pts.abs_diff(delivered.pts.0) <= Duration::from_nanos(2));
    }
}

#[test]
fn destination_authentication_is_not_skipped() {
    let mut tx = ScreenFrameCodec::chacha([0x33; 32], 303, false);
    let mut rx = ScreenFrameCodec::chacha([0x33; 32], 303, true);
    let clock = ControlledClock {
        epoch: Instant::now(),
        offset_ns: 1_000_000_000,
    };
    let packet = ScreenFrame::video_avcc(BytesMut::from(&[0, 0, 0, 2, 0x65, 0x80][..]), clock.epoch, &clock);
    let mut wire = BytesMut::new();
    tx.encode(packet, &mut wire).unwrap();
    *wire.last_mut().unwrap() ^= 1;
    assert!(rx.decode(&mut wire).is_err());
}
