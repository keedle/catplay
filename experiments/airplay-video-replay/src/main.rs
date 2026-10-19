use bytes::BytesMut;
use catplay_airplay_video_replay::video::{AnnexBConverter, avcc_config_deserialize, avcc_config_serialize};

fn main() {
    // Synthetic framing fixtures, not a decodable screen recording.
    let config = [1, 66, 224, 30, 255, 225, 0, 6, 103, 66, 224, 30, 137, 139, 1, 0, 2, 104, 206];
    let parsed = avcc_config_deserialize(&config).expect("synthetic configuration");
    assert_eq!(avcc_config_serialize(&parsed).unwrap(), config);
    let frames: [&[u8]; 2] = [&[0, 0, 0, 3, 0x65, 0x88, 0x84], &[0, 0, 0, 3, 0x41, 0x9a, 0x20]];
    for frame in frames {
        let mut incoming = BytesMut::from(frame);
        let mut receiver = AnnexBConverter::new(4);
        let mut annexb = receiver
            .convert(&mut incoming, false)
            .expect("receive framing");
        let mut transmitter = AnnexBConverter::with_cache(4, receiver.offsets_cache);
        let outgoing = transmitter
            .convert(&mut annexb, true)
            .expect("transmit framing");
        assert_eq!(&outgoing[..], frame);
    }
    println!("Synthetic replay passed: SPS/PPS configuration and IDR/delta NAL bytes preserved by production converters.");
    println!("This is a framing check. No live AirPlay, encryption, decoding or vehicle was tested.");
}
