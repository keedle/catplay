use crate::video::{HevcNalType, NalChunk, NalError, NalType, chunk_payload_bounds};

/// Classify every NAL in an already scanned access unit. Parameter sets, AUD and
/// SEI may precede the random-access picture. This does not decode slice syntax.
pub fn access_unit_is_keyframe(data: &[u8], chunks: &[NalChunk], hevc: bool) -> Result<bool, NalError> {
    let mut found = false;
    let mut h264_delta = false;
    for chunk in chunks {
        let (start, end) = chunk_payload_bounds(data, chunk)?;
        let nal = &data[start..end];
        let first = *nal.first().ok_or(NalError::Param)?;
        if first & 0x80 != 0 {
            return Err(NalError::Param);
        }
        if hevc {
            // HEVC's two-byte NAL header includes a nonzero temporal_id_plus1.
            if nal.len() < 2 || nal[1] & 7 == 0 {
                return Err(NalError::Param);
            }
            found |= HevcNalType::from_byte(first).is_irap();
        } else {
            match NalType::from_byte(first) {
                NalType::IdrSlice if first & 0x60 == 0 => return Err(NalError::Param),
                NalType::IdrSlice => found = true,
                NalType::NonIdrSlice | NalType::DataPartitionA | NalType::DataPartitionB | NalType::DataPartitionC => h264_delta = true,
                NalType::Aud | NalType::Sei if first & 0x60 != 0 => return Err(NalError::Param),
                _ => {}
            }
        }
    }
    if !hevc && found && h264_delta {
        return Err(NalError::Param);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::AnnexBConverter;

    fn classify(nals: &[&[u8]], hevc: bool) -> Result<bool, NalError> {
        let mut wire = Vec::new();
        for nal in nals {
            wire.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            wire.extend_from_slice(nal);
        }
        let chunks = AnnexBConverter::scan_only(&wire, 4, true)?;
        access_unit_is_keyframe(&wire, &chunks, hevc)
    }

    #[test]
    fn h264_idr_after_metadata_is_a_keyframe() {
        assert!(
            classify(
                &[&[0x09, 0xf0], &[0x06, 0x80], &[0x67, 66, 0, 31], &[0x68, 0x80], &[0x65, 0x80]],
                false
            )
            .unwrap()
        );
        assert!(!classify(&[&[0x09, 0xf0], &[0x06, 0x80], &[0x41, 0x80]], false).unwrap());
        assert!(!classify(&[&[0x67, 66, 0, 31], &[0x68, 0x80]], false).unwrap());
    }

    #[test]
    fn hevc_irap_after_parameter_sets_is_a_keyframe() {
        assert!(classify(&[&[32 << 1, 1], &[33 << 1, 1], &[34 << 1, 1], &[19 << 1, 1]], true).unwrap());
        assert!(!classify(&[&[35 << 1, 1], &[1 << 1, 1]], true).unwrap());
    }

    #[test]
    fn invalid_headers_after_idr_are_still_checked() {
        assert!(classify(&[&[0x65, 0x80], &[]], false).is_err());
        assert!(classify(&[&[0x65, 0x80], &[0xe5]], false).is_err());
        assert!(classify(&[&[19 << 1]], true).is_err());
        assert!(classify(&[&[19 << 1, 0]], true).is_err());
        assert!(classify(&[&[0x05, 0x80]], false).is_err());
        assert!(classify(&[&[0x65, 0x80], &[0x41, 0x80]], false).is_err());
        assert!(classify(&[&[0x65, 0x80], &[0x42, 0x80]], false).is_err());
        assert!(classify(&[&[0x69, 0xf0], &[0x65, 0x80]], false).is_err());
        assert!(classify(&[&[0x66, 0x80], &[0x65, 0x80]], false).is_err());
    }
}
