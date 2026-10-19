/// Explicit process-wide phone receiver selection. CarPlay is the default.
#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub mirroring: Option<MirroringOptions>,
}

/// Caller-selected SPS header gate, not measured head-unit decoder support.
#[cfg(feature = "experimental_airplay_mirroring")]
#[derive(Debug, Clone, Copy)]
pub struct MirroringOptions {
    pub h264_profile_idc: u8,
    pub h264_level_idc: u8,
}

impl RuntimeOptions {
    pub fn is_mirroring(&self) -> bool {
        #[cfg(feature = "experimental_airplay_mirroring")]
        return self.mirroring.is_some();
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        false
    }

    pub fn parse(args: &[String]) -> Result<Self, &'static str> {
        if args.is_empty() {
            return Ok(Self::default());
        }
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        return Err("experimental mirroring support was not compiled in");
        #[cfg(feature = "experimental_airplay_mirroring")]
        {
            let mut enabled = false;
            let mut profile = None;
            let mut level = None;
            let mut args = args.iter();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--experimental-airplay-mirroring" if !enabled => enabled = true,
                    "--mirror-h264-profile-idc" if profile.is_none() => {
                        profile = Some(
                            args.next()
                                .ok_or("missing H264 profile value")?
                                .parse::<u8>()
                                .map_err(|_| "invalid H264 profile value")?,
                        );
                    }
                    "--mirror-h264-level-idc" if level.is_none() => {
                        level = Some(
                            args.next()
                                .ok_or("missing H264 level value")?
                                .parse::<u8>()
                                .map_err(|_| "invalid H264 level value")?,
                        );
                    }
                    _ => return Err("unknown or duplicate option"),
                }
            }
            if !enabled {
                return Err("H264 header values require explicit mirroring opt-in");
            }
            let profile = profile.ok_or("mirroring requires an explicit H264 profile_idc")?;
            let level = level.ok_or("mirroring requires an explicit H264 level_idc")?;
            if ![66, 77, 100].contains(&profile) || ![10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42].contains(&level) {
                return Err("H264 header values are outside the experimental subset");
            }
            Ok(Self {
                mirroring: Some(MirroringOptions {
                    h264_profile_idc: profile,
                    h264_level_idc: level,
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn no_options_preserves_carplay() {
        assert!(!RuntimeOptions::parse(&[]).unwrap().is_mirroring());
    }
    #[test]
    fn unknown_options_fail_closed() {
        assert!(RuntimeOptions::parse(&args(&["--unknown"])).is_err());
    }
    #[cfg(not(feature = "experimental_airplay_mirroring"))]
    #[test]
    fn runtime_flag_cannot_enable_uncompiled_receiver() {
        assert!(RuntimeOptions::parse(&args(&["--experimental-airplay-mirroring"])).is_err());
    }
    #[cfg(feature = "experimental_airplay_mirroring")]
    #[test]
    fn mirroring_requires_flag_and_both_header_values() {
        let valid = [
            "--experimental-airplay-mirroring",
            "--mirror-h264-profile-idc",
            "66",
            "--mirror-h264-level-idc",
            "31",
        ];
        assert!(RuntimeOptions::parse(&args(&valid)).unwrap().is_mirroring());
        assert!(RuntimeOptions::parse(&args(&valid[1..])).is_err());
        assert!(RuntimeOptions::parse(&args(&valid[..3])).is_err());
        let mut duplicate = args(&valid);
        duplicate.push(valid[0].into());
        assert!(RuntimeOptions::parse(&duplicate).is_err());
        let mut invalid = args(&valid);
        invalid[2] = "255".into();
        assert!(RuntimeOptions::parse(&invalid).is_err());
    }
}
