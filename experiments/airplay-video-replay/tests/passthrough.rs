use bytes::BytesMut;
use catplay_airplay_video_replay::video::*;

fn pack(nals: &[&[u8]], length: usize) -> BytesMut {
    let mut out = BytesMut::new();
    for nal in nals {
        let size = (nal.len() as u32).to_be_bytes();
        out.extend_from_slice(&size[4 - length..]);
        out.extend_from_slice(nal);
    }
    out
}

#[test]
fn receive_to_transmit_preserves_order_and_bytes_across_prefix_widths() {
    // These are synthetic framing fixtures, not decoder-valid pictures.
    let nals: &[&[u8]] = &[&[0x09, 0xf0], &[0x06, 0x05, 0x03], &[0x65, 0x88, 0x84], &[0x65, 0x99]];
    for input_width in [1, 2, 4] {
        for output_width in [1, 2, 4] {
            let mut wire = pack(nals, input_width);
            let mut receiver = AnnexBConverter::new(input_width);
            let mut intermediate = receiver.convert(&mut wire, false).unwrap();
            let mut transmitter = AnnexBConverter::new(output_width);
            let outgoing = transmitter.convert(&mut intermediate, true).unwrap();
            assert_eq!(outgoing, pack(nals, output_width));
        }
    }
}

#[test]
fn advertised_configuration_preserves_parameter_sets() {
    let config = [1, 66, 224, 30, 255, 225, 0, 6, 103, 66, 224, 30, 137, 139, 1, 0, 2, 104, 206];
    let received = avcc_config_deserialize(&config).unwrap();
    let outgoing = avcc_config_serialize(&received).unwrap();
    assert_eq!(outgoing, config);
    assert_eq!(received.nal_size_len, 4);
    assert_eq!(received.sps_pps, pack_annexb(&[&[103, 66, 224, 30, 137, 139], &[104, 206]]));
}

fn pack_annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|nal| [0, 0, 0, 1].into_iter().chain(nal.iter().copied()))
        .collect()
}

#[test]
fn truncated_wire_data_fails_instead_of_forwarding_partial_nal() {
    let complete = pack(&[&[0x65, 0x88, 0x84, 0xff]], 4);
    for length in 1..complete.len() {
        let mut partial = BytesMut::from(&complete[..length]);
        assert!(
            AnnexBConverter::new(4)
                .convert(&mut partial, false)
                .is_err()
        );
    }
}

#[test]
fn corrupt_offsets_cannot_change_transmitted_payload() {
    let mut payload = BytesMut::from(&[0, 0, 0, 1, 0x65, 0x88, 0x84][..]);
    let cache = vec![NalChunk {
        prefix_start: 0,
        prefix_len: 4,
        data_size: usize::MAX,
    }];
    assert!(
        AnnexBConverter::with_cache(4, cache)
            .convert(&mut payload, true)
            .is_err()
    );
}

#[test]
fn smaller_prefix_rejects_payload_that_does_not_fit() {
    let mut payload = BytesMut::from(&[0, 0, 0, 1][..]);
    payload.extend_from_slice(&vec![0x65; 256]);
    assert!(AnnexBConverter::new(1).convert(&mut payload, true).is_err());
}
