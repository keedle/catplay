//! Explicit, bounded receiver-only protocol probe. Build/test does not run it.
//! No car transmitter, USB/Bluetooth, persistent pairing, raw logger or media dump.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use catplay_carplay::{
    carplay_rx::{
        AirPlayInitialSetupObservation, AirPlayReceiverDiagnostic, AirPlayReceiverHandleRef, AirPlayReceiverProfile, AirPlayReceiverSink,
        sink::{AirPlayServer, AirPlayServerShared},
    },
    msg::{Display, InfoMessageResponse},
    rtsp_frame::{RtspError, RtspResult},
    screen::rx::{ScreenReceiverSink, ScreenReceiverSinkBox},
    video::{AnnexBIter, AvccConfigExtended, EncodedVideoFrame},
};
use catplay_hap::HomekitStorageFile;
use catplay_tracing::tracer::SessionTracer;
use catplay_util::{AsyncShutdown, EventReconciler, EventSleeper};
use macaddr::MacAddr6;

#[derive(Debug, PartialEq, Eq)]
struct Options {
    interface: String,
    width: u32,
    height: u32,
    seconds: u64,
}

fn parse_options(args: &[String]) -> Result<Options, &'static str> {
    let mut enabled = false;
    let mut interface = None;
    let mut width = None;
    let mut height = None;
    let mut seconds = None;
    let mut cursor = 0;
    while cursor < args.len() {
        let flag = args[cursor].as_str();
        if flag == "--enable-mirroring" {
            if enabled {
                return Err("duplicate opt-in flag");
            }
            enabled = true;
            cursor += 1;
            continue;
        }
        let value = args.get(cursor + 1).ok_or("missing option value")?;
        match flag {
            "--interface" if interface.is_none() => {
                if value.is_empty()
                    || value.len() > 15
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
                {
                    return Err("invalid interface name");
                }
                interface = Some(value.clone());
            }
            "--width" if width.is_none() => width = Some(value.parse().map_err(|_| "invalid width")?),
            "--height" if height.is_none() => height = Some(value.parse().map_err(|_| "invalid height")?),
            "--seconds" if seconds.is_none() => seconds = Some(value.parse().map_err(|_| "invalid duration")?),
            _ => return Err("unknown or duplicate option"),
        }
        cursor += 2;
    }
    if !enabled {
        return Err("explicit --enable-mirroring is required");
    }
    let options = Options {
        interface: interface.ok_or("--interface is required")?,
        width: width.ok_or("--width is required")?,
        height: height.ok_or("--height is required")?,
        seconds: seconds.unwrap_or(30),
    };
    if options.width == 0 || options.height == 0 || options.width > 4096 || options.height > 4096 {
        return Err("probe dimensions must be within 1..4096");
    }
    if !(1..=120).contains(&options.seconds) {
        return Err("duration must be within 1..120 seconds");
    }
    Ok(options)
}

#[derive(Default)]
struct Counters {
    initial_setups: u64,
    accepted_sessions: u64,
    frames: u64,
    compressed_bytes: u64,
    idr_frames: u64,
    configs: u64,
    diagnostics: u64,
    reports: usize,
}

impl Counters {
    fn report(&mut self, message: std::fmt::Arguments<'_>) {
        if self.reports < 128 {
            println!("{message}");
            self.reports += 1;
        }
    }
}

struct ProbeSink {
    width: u32,
    height: u32,
    counters: Arc<Mutex<Counters>>,
}

#[async_trait]
impl AirPlayReceiverSink for ProbeSink {
    fn init(&mut self, handle: AirPlayReceiverHandleRef) -> RtspResult<()> {
        let mut counters = self.counters.lock().unwrap();
        counters.accepted_sessions += 1;
        counters.report(format_args!(
            "session_admitted mirroring={} carplay={}",
            handle.is_mirroring(),
            handle.is_car()
        ));
        Ok(())
    }

    fn on_initial_setup_observation(&mut self, observation: AirPlayInitialSetupObservation) {
        let mut counters = self.counters.lock().unwrap();
        counters.initial_setups += 1;
        counters.report(format_args!(
            "initial_setup timing={:?} stream_count={} first_stream_types={:?} encryption_type_bits={:?} screen_mirroring={}",
            observation.timing_protocol,
            observation.stream_count,
            observation.stream_types,
            observation.encryption_type_bits,
            observation.is_screen_mirroring_session
        ));
    }

    fn on_receiver_diagnostic(&mut self, diagnostic: AirPlayReceiverDiagnostic) {
        let mut counters = self.counters.lock().unwrap();
        counters.diagnostics += 1;
        counters.report(format_args!("receiver_diagnostic kind={diagnostic:?}"));
    }

    async fn on_info(&mut self, info: &mut InfoMessageResponse) {
        info.displays = vec![Display {
            width_pixels: self.width,
            height_pixels: self.height,
            max_fps: Some(30),
            uuid: "catplay-mirror-probe-display".into(),
            ..Default::default()
        }];
    }

    async fn open_screen(&mut self, _latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        Ok(Box::new(MetadataScreen {
            counters: self.counters.clone(),
            last_report: Instant::now(),
            shutdown: false,
        }))
    }
}

impl EventSleeper for ProbeSink {}
impl EventReconciler for ProbeSink {
    type Error = RtspError;
}
impl AsyncShutdown for ProbeSink {}

struct MetadataScreen {
    counters: Arc<Mutex<Counters>>,
    last_report: Instant,
    shutdown: bool,
}

#[async_trait]
impl ScreenReceiverSink for MetadataScreen {
    async fn init(&mut self) -> RtspResult<()> {
        Ok(())
    }

    async fn set_avcc_config(&mut self, config: AvccConfigExtended) -> RtspResult<()> {
        let mut counters = self.counters.lock().unwrap();
        counters.configs += 1;
        // Parameter-set bytes are intentionally never printed or retained.
        if counters.configs == 1 || self.last_report.elapsed() >= Duration::from_secs(5) {
            counters.report(format_args!(
                "video_config codec=H264 width={} height={} nal_length_bytes={} respect_timestamps={}",
                config.width, config.height, config.avcc.nal_size_len, config.respect_timestamps
            ));
            let sps_header = AnnexBIter::annexb(&config.avcc.sps_pps)
                .filter_map(Result::ok)
                .find_map(|nal| {
                    let start = nal.prefix_start + nal.prefix_len;
                    let bytes = config.avcc.sps_pps.get(start..start + nal.data_size)?;
                    (bytes.len() >= 4 && bytes[0] & 0x1f == 7).then(|| (bytes[1], bytes[2], bytes[3]))
                });
            if let Some((profile, constraints, level)) = sps_header {
                counters.report(format_args!("sps_header_observation profile_idc={profile} constraint_flags={constraints} level_idc={level} semantics_validated=false"));
            }
            self.last_report = Instant::now();
        }
        Ok(())
    }

    async fn process_frame(&mut self, frame: EncodedVideoFrame) -> RtspResult<()> {
        let mut counters = self.counters.lock().unwrap();
        counters.frames += 1;
        let is_idr = AnnexBIter::annexb(&frame.data)
            .filter_map(Result::ok)
            .any(|nal| {
                frame
                    .data
                    .get(nal.prefix_start + nal.prefix_len)
                    .is_some_and(|byte| byte & 0x1f == 5)
            });
        counters.idr_frames += u64::from(is_idr);
        counters.compressed_bytes = counters
            .compressed_bytes
            .saturating_add(frame.data.len() as u64);
        if counters.frames == 1 || self.last_report.elapsed() >= Duration::from_secs(5) {
            let (frames, bytes, idrs) = (counters.frames, counters.compressed_bytes, counters.idr_frames);
            counters.report(format_args!(
                "video_progress frames={frames} idr_frames={idrs} compressed_bytes={bytes}"
            ));
            self.last_report = Instant::now();
        }
        // frame and its encoded bytes are dropped here, never queued or decoded.
        Ok(())
    }
}

impl AsyncShutdown for MetadataScreen {
    async fn shutdown(&mut self) {
        if !self.shutdown {
            self.shutdown = true;
            self.counters
                .lock()
                .unwrap()
                .report(format_args!("video_stream_closed"));
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Existing receiver trace/debug paths can contain keys. Never install the
    // normal CatPlay logger, even if RUST_LOG is set, and disable session files.
    SessionTracer::disable_globally();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "Usage: airplay_mirror_probe --enable-mirroring --interface IFACE --width PIXELS --height PIXELS [--seconds 1..120]\nDimensions are explicit probe settings, not measured vehicle capabilities.\nThis listens on the selected Linux host interface. It never forwards media to a car."
        );
        return;
    }
    let options = match parse_options(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}; use --help");
            std::process::exit(2);
        }
    };
    if let Err(message) = run(options).await {
        eprintln!("probe_failed stage={message}");
        std::process::exit(1);
    }
}

async fn run(options: Options) -> Result<(), &'static str> {
    let counters = Arc::new(Mutex::new(Counters::default()));
    let sink_counters = counters.clone();
    let shared = AirPlayServerShared::new();
    let mut identity: [u8; 6] = rand::random();
    identity[0] = (identity[0] & 0xfc) | 0x02;
    let mut server = AirPlayServer::new(
        7001,
        MacAddr6::from(identity),
        &options.interface,
        HomekitStorageFile::memory(),
        None,
        shared.clone(),
        AirPlayReceiverProfile::AppleTVH264VideoOnly,
        move || ProbeSink {
            width: options.width,
            height: options.height,
            counters: sink_counters.clone(),
        },
    );
    server.bind().await.map_err(|_| "bind")?;
    server.start_advertise().map_err(|_| "discovery")?;
    println!(
        "probe_listening port=7001 seconds={} requested_width={} requested_height={} storage=in_memory audio=refused forwarding=none",
        options.seconds, options.width, options.height
    );
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(options.seconds)) => {},
        result = tokio::signal::ctrl_c() => {
            if result.is_err() {
                // Continue to bounded cleanup even if the signal handler failed.
                eprintln!("probe_stop reason=signal_handler_error");
            }
        }
    }
    if let Some(handle) = shared.borrow() {
        handle.close(RtspError::UserClosing);
    }
    tokio::time::timeout(Duration::from_secs(3), server.shutdown())
        .await
        .map_err(|_| "shutdown_timeout")?;
    let counters = counters.lock().unwrap();
    println!(
        "probe_summary initial_setups={} admitted_sessions={} configs={} frames={} idr_frames={} compressed_bytes={} diagnostics={} active_session={}",
        counters.initial_setups,
        counters.accepted_sessions,
        counters.configs,
        counters.frames,
        counters.idr_frames,
        counters.compressed_bytes,
        counters.diagnostics,
        shared.borrow().is_some()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).into()).collect()
    }

    #[test]
    fn parser_requires_explicit_opt_in_interface_and_dimensions() {
        assert!(parse_options(&[]).is_err());
        assert!(parse_options(&args(&["--interface", "lo", "--width", "800", "--height", "480"])).is_err());
        assert!(parse_options(&args(&["--enable-mirroring", "--interface", "lo"])).is_err());
        assert_eq!(
            parse_options(&args(&[
                "--enable-mirroring",
                "--interface",
                "lo",
                "--width",
                "800",
                "--height",
                "480"
            ]))
            .unwrap(),
            Options {
                interface: "lo".into(),
                width: 800,
                height: 480,
                seconds: 30
            }
        );
    }

    #[test]
    fn parser_refuses_unbounded_or_ambiguous_options() {
        let duplicate_duration = args(&[
            "--enable-mirroring",
            "--interface",
            "lo",
            "--width",
            "800",
            "--height",
            "480",
            "--seconds",
            "10",
            "--seconds",
            "20",
        ]);
        assert!(parse_options(&duplicate_duration).is_err());
        for extra in [["--seconds", "0"], ["--seconds", "121"], ["--width", "9999"], ["--unknown", "x"]] {
            let mut values = args(&["--enable-mirroring", "--interface", "lo", "--width", "800", "--height", "480"]);
            values.extend(args(&extra));
            assert!(parse_options(&values).is_err());
        }
    }
}
