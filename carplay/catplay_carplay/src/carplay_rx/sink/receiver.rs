use async_trait::async_trait;
use catplay_hap::{HomekitStorage, HomekitStorageRef};

use catplay_iap2_client::{
    CsmRemote,
    tokio::{AsyncClient, AsyncClientDrain},
};
use catplay_mfi::MfiDevice;
use catplay_plist::CachingSerializer;
use catplay_tokio::{TcpHelper, UdpHelper};
use catplay_util::{AsyncShutdown, EventReconciler, EventSink, EventSleeper, futures_xordered::FuturesUnordered, mpsc, spawn_fork};
use futures::future::BoxFuture;
use log::{debug, error, info, trace, warn};
use macaddr::MacAddr6;
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Instant,
};
use std::{sync::Arc, time::Duration};

use crate::{
    audio::codec::{AudioDecoder, AudioEncoder},
    carplay_rx::{
        AirPlayInitialSetupObservation, AirPlayObservedTimingProtocol, AirPlayReceiverDiagnostic, AirPlayReceiverHandleRef,
        AirPlayReceiverProfile, AirPlayReceiverSessionError, AirPlayReceiverSinkBox,
        sink::{AirPlayReceiverHandleImpl, AirPlayServerShared, AirPlaySessionGuard},
    },
    cipher::AirPlayStreamEncryption,
    clock::{MediaClockProxy, TimingClient},
    common::{AIRPLAY_SDK_VERSION, AirPlayBonjourEntry, AirPlayEncryptionType, AirPlayFeature, AirPlayStatus},
    events::EventsClient,
    keep_alive::KeepAliveServer,
    modes::AirPlayModeState,
    msg::{
        AudioFormat, AudioFormatStruct, AudioLatency, AudioType, Command, CommandError, CommandIApSendMessage, ControllerFeature,
        ExtendedFeature, HevcInfo, InfoMessage, InfoMessageResponse, InfoMessageTxtAirPlayResponse, InitialSetup, InitialSetupResponse,
        Setup, SetupResponse, StreamDescription, StreamDescriptionAudio, StreamDescriptionResponse, StreamDescriptionScreen, StreamType,
        TeardownPayload,
    },
    pairing::PairingHelperRx,
    rtp::RtpReceiver,
    rtsp_frame::{HttpHeader, HttpStatus, RtspError, RtspMethod::*, RtspQueue, RtspRequest, RtspResponse, RtspResult},
    rtsp_session::{RtspReceiverCallback, RtspReceiverEvent, RtspReceiverEventResult},
    screen::rx::ScreenReceiverSession,
};

#[derive(Default, PartialEq, Debug)]
enum AirPlayReceiverState {
    #[default]
    Pairing,
    InitialSetup,
    PreRecord,
    PostRecord,

    Closed(RtspError),
}

impl AirPlayReceiverState {
    pub fn change(&mut self, next: Self) {
        debug!("State {self:?} -> {next:?}");
        *self = next;
    }
}

#[derive(EventSleeper, EventReconciler)]
#[reconcile_error(RtspError)]
#[reconcile_func(reconcile_close_status, |e: AirPlayReceiverSessionError| RtspError::foreign(e))]
#[reconcile_func(flush_iap2)]
#[reconcile_func(reconcile_event_loop)]
#[reconcile_func(reconcile_mirroring_screen_status)]
pub struct AirPlayReceiver {
    pub bind_ip: SocketAddr,
    pub peer_ip: SocketAddr,
    iface: String,
    mac_addr: MacAddr6,

    #[sleep]
    event_loop: FuturesUnordered<BoxFuture<'static, ()>>,
    task_queue_tx: mpsc::UnboundedSender<BoxFuture<'static, ()>>,
    #[sleep]
    task_queue_rx: mpsc::UnboundedReceiver<BoxFuture<'static, ()>>,

    #[sleep]
    sink: AirPlayReceiverSinkBox,
    sink_had_init: bool,

    shared: AirPlayServerShared,
    pub lock: Option<AirPlaySessionGuard>,

    pub homekit: HomekitStorageRef,
    pub pairing: PairingHelperRx,

    state: AirPlayReceiverState,
    stream_encryption: AirPlayStreamEncryption,
    #[sleep]
    #[reconcile(|e: AirPlayReceiverSessionError| RtspError::foreign(e))]
    streams: Streams,

    #[sleep]
    close_pending_rx: mpsc::Receiver<RtspError>,
    close_pending_tx: mpsc::Sender<RtspError>,

    wireless: bool,

    modes: AirPlayModeState,
    profile: AirPlayReceiverProfile,
    features: AirPlayFeature,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirroring_audio_formats: AudioFormat,
    serializer: CachingSerializer,
    start: Instant,
}

#[derive(Default, AsyncShutdown, EventSleeper, EventReconciler)]
#[reconcile_error(AirPlayReceiverSessionError)]
struct Streams {
    #[sleep]
    #[reconcile(AirPlayReceiverSessionError::KeepAlive)]
    #[shutdown(take)]
    keep_alive_socket: Option<UdpHelper<1500, 0, KeepAliveServer>>,
    #[sleep]
    #[reconcile(AirPlayReceiverSessionError::Events)]
    #[shutdown(take)]
    event_socket: Option<TcpHelper<EventsClient>>,
    #[sleep]
    #[reconcile(AirPlayReceiverSessionError::Timing)]
    #[shutdown(take)]
    timing_socket: Option<UdpHelper<1500, 0, TimingClient>>,
    media_clock: Option<MediaClockProxy>,

    #[sleep]
    #[shutdown(take)]
    // #[reconcile(AirPlayReceiverSessionError::Screen)]
    screen: Option<TcpHelper<ScreenReceiverSession>>,
    #[shutdown(take)]
    #[sleep]
    #[reconcile(AirPlayReceiverSessionError::Rtp)]
    main_audio: Option<RtpReceiver>,
    #[shutdown(take)]
    #[sleep]
    #[reconcile(AirPlayReceiverSessionError::Rtp)]
    alt_audio: Option<RtpReceiver>,

    event_rtsp: Option<RtspQueue>,

    #[sleep]
    #[shutdown(take)]
    #[reconcile]
    iap2: Option<AsyncClient>,
    #[sleep]
    iap2_drain: Option<AsyncClientDrain>,

    handle: Option<AirPlayReceiverHandleRef>,
}

#[async_trait]
impl EventSink<RtspReceiverEvent<'_>, RtspReceiverEventResult> for AirPlayReceiver {
    async fn on_event<'a>(&mut self, event: RtspReceiverEvent<'a>) -> RtspReceiverEventResult {
        #[cfg(debug_assertions)]
        trace!("Received event: {event:?}");

        match event {
            RtspReceiverEvent::SetBindIp(bind_ip) => self.bind_ip = bind_ip,
            RtspReceiverEvent::SetPeerIp(peer_ip) => self.peer_ip = peer_ip,
            RtspReceiverEvent::Request { request, response } => {
                if let Err(err) = self.process_request(request, response).await {
                    return if let Some(key) = self.pairing.transition_pending_to_encrypted() {
                        self.state.change(AirPlayReceiverState::InitialSetup);
                        RtspReceiverEventResult::encrypt(Err(err), key)
                    } else {
                        RtspReceiverEventResult::new(Err(err))
                    };
                }

                return if let Some(key) = self.pairing.transition_pending_to_encrypted() {
                    self.state.change(AirPlayReceiverState::InitialSetup);
                    RtspReceiverEventResult::encrypt_ready(key)
                } else {
                    RtspReceiverEventResult::ready()
                };
            }
            RtspReceiverEvent::Eof(err) => {
                warn!("Observed EOF on RTSP stream! {err:?}");
                self.shutdown().await;
            }
        }

        RtspReceiverEventResult::noop()
    }
}

impl RtspReceiverCallback for AirPlayReceiver {}

impl AsyncShutdown for AirPlayReceiver {
    async fn shutdown(&mut self) {
        self.streams.shutdown().await;
        self.streams = Streams::default();
        if std::mem::take(&mut self.sink_had_init) {
            self.sink.shutdown().await;
        }
        // The sink can publish its disconnect while still owning the lock.
        // A later Drop must not publish a second, stale disconnect.
        self.lock.take();
    }
}

impl Drop for AirPlayReceiver {
    fn drop(&mut self) {
        debug!(
            "AirPlayReceiver {}<->{} dropped in state {:?}",
            self.bind_ip, self.peer_ip, self.state
        );
    }
}

impl AirPlayReceiver {
    const ALLOW_PAIR_SETUP_DOWNGRADE: bool = true;
    const TCP_REVERSE_CONN_TIMEOUT: Duration = Duration::from_millis(2000);
    const CARPLAY_FORCE_HEVC: bool = false;
    const APPLETV_FORCE_HEVC: bool = true;

    pub fn new(
        iface: &str,
        mac_addr: MacAddr6,
        homekit: Arc<dyn HomekitStorage>,
        mfi: Option<Arc<dyn MfiDevice>>,
        shared: AirPlayServerShared,
        sink: AirPlayReceiverSinkBox,
        profile: AirPlayReceiverProfile,
    ) -> Self {
        let pairing = PairingHelperRx::new(homekit.clone(), mfi);
        let (close_pending_tx, close_pending_rx) = mpsc::channel(1);

        let event_loop = mpsc::unbounded();

        AirPlayReceiver {
            bind_ip: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
            peer_ip: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
            iface: iface.into(),
            mac_addr,

            event_loop: Default::default(),
            task_queue_tx: event_loop.0,
            task_queue_rx: event_loop.1,

            shared,
            lock: None,

            homekit,
            pairing,

            streams: Streams::default(),
            sink,
            sink_had_init: false,

            state: AirPlayReceiverState::InitialSetup, /*Pairing*/
            stream_encryption: AirPlayStreamEncryption::Unconfigured,
            close_pending_rx,
            close_pending_tx,
            wireless: true, // TODO
            modes: Default::default(),
            profile,
            features: Self::features(profile),
            #[cfg(feature = "experimental_airplay_mirroring")]
            mirroring_audio_formats: AudioFormat::empty(),
            serializer: CachingSerializer::default(),
            start: Instant::now(),
        }
    }

    async fn reconcile_event_loop(&mut self) -> RtspResult<()> {
        if let Some(task) = self.task_queue_rx.take() {
            self.event_loop.push(task);
        }

        self.event_loop.take();
        Ok(())
    }

    async fn reconcile_mirroring_screen_status(&mut self) -> RtspResult<()> {
        if self.profile.is_experimental_mirroring()
            && let Some(screen) = self.streams.screen.as_mut()
            && let Err(error) = screen.reconcile().await
        {
            self.sink
                .on_receiver_diagnostic(if matches!(&error, Some(RtspError::PayloadTooBig(..))) {
                    AirPlayReceiverDiagnostic::ScreenPayloadTooLarge
                } else {
                    AirPlayReceiverDiagnostic::ScreenTerminated
                });
            return Err(error.unwrap_or(RtspError::Closed));
        }
        Ok(())
    }

    async fn reconcile_close_status(&mut self) -> Result<(), AirPlayReceiverSessionError> {
        if let Some(err) = self.close_pending_rx.take() {
            warn!("Closing receiver as requested: {err}");
            self.state = AirPlayReceiverState::Closed(err);
        }

        if let AirPlayReceiverState::Closed(err) = &self.state {
            return Err(AirPlayReceiverSessionError::Sink(err.clone()));
        }

        Ok(())
    }

    pub async fn process_request(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        let (method, url) = (req.method, req.url.clone());

        if !Self::ALLOW_PAIR_SETUP_DOWNGRADE && self.features.contains(AirPlayFeature::CAR) && !self.pairing.allow_unpaired(req) {
            return Err(RtspError::NotEncrypted);
        }

        match (method, url.as_str()) {
            (Options, _) => self.respond_options(req, resp),
            (GetParameter, _) => self.respond_get_parameter(req, resp)?,
            (SetParameter, _) => self.respond_set_parameter(req, resp)?,

            (Flush, _) => self.respond_flush(req, resp)?,
            (Record, _) => self.respond_record(req, resp).await?,
            (Setup, _) => self.respond_setup(req, resp).await?,
            (Teardown, _) => self.respond_teardown(req, resp).await?,

            // Gets
            (Get, "/info") => self.respond_info(req, resp).await?,
            (Get, "/log") => Err(HttpStatus::NotFound)?,
            (Get, _) => Err(HttpStatus::NotFound)?,

            // Posts
            (Post, "/pair-setup") => {
                let result = self.pairing.respond_pair_setup(req, resp).await;
                if self.profile.is_experimental_mirroring() {
                    self.sink.on_receiver_diagnostic(if result.is_ok() {
                        AirPlayReceiverDiagnostic::PairSetupHandlerCompleted
                    } else {
                        AirPlayReceiverDiagnostic::PairSetupHandlerFailed
                    });
                }
                result?;
            }
            (Post, "/pair-verify") => {
                let result = self.pairing.respond_pair_verify(req, resp).await;
                if self.profile.is_experimental_mirroring() {
                    self.sink.on_receiver_diagnostic(if result.is_ok() {
                        AirPlayReceiverDiagnostic::PairVerifyHandlerCompleted
                    } else {
                        AirPlayReceiverDiagnostic::PairVerifyHandlerFailed
                    });
                }
                result?;
            }
            (Post, "/auth-setup") => {
                let result = self.pairing.respond_auth_setup(req, resp).await;
                if self.profile.is_experimental_mirroring() {
                    self.sink.on_receiver_diagnostic(if result.is_ok() {
                        AirPlayReceiverDiagnostic::AuthSetupHandlerCompleted
                    } else {
                        AirPlayReceiverDiagnostic::AuthSetupHandlerFailed
                    });
                }
                result?;
            }
            (Post, "/fp-setup") => {
                let result = self.pairing.respond_fp_setup(req, resp);
                if self.profile.is_experimental_mirroring() {
                    self.sink.on_receiver_diagnostic(if result.is_ok() {
                        AirPlayReceiverDiagnostic::FairPlayHandlerCompleted
                    } else {
                        AirPlayReceiverDiagnostic::FairPlayHandlerFailed
                    });
                }
                result?;
            }

            (Post, "/command") => self.respond_command(req, resp).await?,
            (Post, "/diag-info") => Err(HttpStatus::NotFound)?,
            (Post, "/feedback") => Err(HttpStatus::Ok)?,
            (Post, "/info") => self.respond_info(req, resp).await?,
            (Post, "/audio-mode") => Err(HttpStatus::Ok)?,
            (Post, _) => Err(HttpStatus::NotFound)?,

            (_, _) => Err(HttpStatus::NotFound)?,
        };

        Ok(())
    }

    fn respond_get_parameter(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        if req.get_header(&HttpHeader::ContentType) == Some("text/parameters") {
            let body = String::from_utf8_lossy(&req.payload);
            let mut out = String::new();

            for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
                match line {
                    "volume" => out.push_str("volume: 0.000000\r\n"),
                    _ => {}
                }
            }

            if !out.is_empty() {
                resp.payload.clear();
                resp.payload.extend_from_slice(out.as_bytes());
                resp.set_header(HttpHeader::ContentType, "text/parameters");
            }
        }

        Ok(())
    }

    fn respond_set_parameter(&mut self, req: &RtspRequest, _resp: &mut RtspResponse) -> RtspResult<()> {
        if req.get_header(&HttpHeader::ContentType) == Some("text/parameters") {
            let body = String::from_utf8_lossy(&req.payload);
            warn!("Ignoring AirPlay SET_PARAMETER payload: {}", body.trim_end());
        }

        Ok(())
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    fn validate_mirroring_streams(&self, streams: &[StreamDescription]) -> RtspResult<()> {
        let mut screen_count = usize::from(self.streams.screen.is_some());
        let mut audio_count = usize::from(self.streams.main_audio.is_some());
        for stream in streams {
            match stream {
                StreamDescription::Screen(screen) if screen.stream_type == StreamType::Screen => {
                    screen_count += 1;
                    if screen_count > 1 {
                        return Err(RtspError::ProtocolViolation("mirroring supports one screen stream"));
                    }
                }
                StreamDescription::Audio(audio) => {
                    audio_count += 1;
                    if audio_count > 1 || self.validate_mirroring_audio(audio).is_err() {
                        return Err(RtspError::ProtocolViolation(
                            "mirroring supports one negotiated output-only media audio stream",
                        ));
                    }
                }
                StreamDescription::AudioLegacy(audio) => {
                    audio_count += 1;
                    if audio_count > 1 || (!audio.is_media && !audio.using_screen) || audio.stream_type != StreamType::GeneralAudio {
                        return Err(RtspError::ProtocolViolation("unsupported legacy mirroring audio"));
                    }
                    let mut modern = audio.as_modern();
                    modern.audio_type = AudioType::Media;
                    self.validate_mirroring_audio(&modern)?;
                }
                _ => return Err(RtspError::ProtocolViolation("unsupported mirroring stream")),
            }
        }
        Ok(())
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    fn validate_mirroring_audio(&self, audio: &StreamDescriptionAudio) -> RtspResult<()> {
        if audio.input
            || audio.audio_type != AudioType::Media && audio.audio_type != AudioType::Default
            || !matches!(
                audio.stream_type,
                StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio
            )
            || audio.audio_format.bits().count_ones() != 1
            || !self.mirroring_audio_formats.contains(audio.audio_format)
        {
            return Err(RtspError::ProtocolViolation("unsupported mirroring audio format"));
        }
        Ok(())
    }

    pub fn features(profile: AirPlayReceiverProfile) -> AirPlayFeature {
        let mut features: AirPlayFeature = AirPlayFeature::empty();

        match profile {
            AirPlayReceiverProfile::CarPlay => {
                features |= AirPlayFeature::AUDIO;
                // features |= AirPlayFeature::REDUNDANT_AUDIO;
                features |= AirPlayFeature::AUDIO_AES_128_MFI_SAP_V1;
                features |= AirPlayFeature::AUDIO_PCM;
                features |= AirPlayFeature::AUDIO_UNENCRYPTED;
                features |= AirPlayFeature::SCREEN;
                features |= AirPlayFeature::ROTATE;
                features |= AirPlayFeature::UNIFIED_BONJOUR;
                features |= AirPlayFeature::CAR;
                features |= AirPlayFeature::CARPLAY_CONTROL;
                features |= AirPlayFeature::HK_PAIRING_AND_ENCRYPT;

                features |= AirPlayFeature::AUDIO_AAC_LC;
            }
            #[cfg(feature = "experimental_airplay_mirroring")]
            AirPlayReceiverProfile::AppleTVH264VideoOnly => {
                features |= AirPlayFeature::SCREEN;
                features |= AirPlayFeature::UNIFIED_BONJOUR;
                features |= AirPlayFeature::VIDEO_FAIRPLAY;
                features |= AirPlayFeature::AUTHENTICATION4;
            }
            #[cfg(feature = "experimental_airplay_mirroring")]
            AirPlayReceiverProfile::AppleTVH264Mirroring => {
                features |= AirPlayFeature::AUDIO;
                features |= AirPlayFeature::AUDIO_PCM;
                features |= AirPlayFeature::SCREEN;
                features |= AirPlayFeature::UNIFIED_BONJOUR;
                features |= AirPlayFeature::AUDIO_AAC_LC;
                features |= AirPlayFeature::AUDIO_AAC_ELD;
                features |= AirPlayFeature::AUDIO_ALAC;
                features |= AirPlayFeature::VIDEO_FAIRPLAY;
                features |= AirPlayFeature::AUTHENTICATION4;
            }
            AirPlayReceiverProfile::AppleTV => {
                features |= AirPlayFeature::AUDIO;
                // features |= AirPlayFeature::REDUNDANT_AUDIO;
                // features |= AirPlayFeature::AUDIO_AES_128_MFI_SAP_V1;
                features |= AirPlayFeature::AUDIO_PCM;
                features |= AirPlayFeature::AUDIO_UNENCRYPTED;
                features |= AirPlayFeature::SCREEN;
                features |= AirPlayFeature::ROTATE;
                features |= AirPlayFeature::UNIFIED_BONJOUR;
                // features |= AirPlayFeature::CAR;
                // features |= AirPlayFeature::CARPLAY_CONTROL;
                // features |= AirPlayFeature::HK_PAIRING_AND_ENCRYPT; // can be enabled for testing

                features |= AirPlayFeature::AUDIO_AAC_LC;
                features |= AirPlayFeature::AUDIO_AAC_ELD;
                features |= AirPlayFeature::AUDIO_ALAC;
                // features |= AirPlayFeature::FPSAP_V2PT5_AES_GCM;
                features |= AirPlayFeature::VIDEO_FAIRPLAY;
                features |= AirPlayFeature::AUTHENTICATION4;

                if Self::APPLETV_FORCE_HEVC {
                    features |= AirPlayFeature::SUPPORTS_SCREEN_MULTI_CODEC; // can be enabled for testing
                }
            }
        }

        features
    }

    pub fn bonjour(profile: AirPlayReceiverProfile, homekit: HomekitStorageRef, device_id: MacAddr6) -> AirPlayBonjourEntry {
        pub const AIRPLAY_MODEL_BRAND_CARPLAY: &str = "CarPlay";
        pub const AIRPLAY_MODEL_BRAND_MIRROR: &str = "AppleTV3,2";

        pub const AIRPLAY_PROTO_VERS: &str = "1.0";

        AirPlayBonjourEntry {
            srcvers: AIRPLAY_SDK_VERSION.into(),
            pi: Some(homekit.device_id()),
            pk: homekit.public_key_as_hex(),
            protovers: AIRPLAY_PROTO_VERS.into(),
            model: match profile {
                AirPlayReceiverProfile::CarPlay => AIRPLAY_MODEL_BRAND_CARPLAY,
                _ => AIRPLAY_MODEL_BRAND_MIRROR,
            }
            .into(),
            features: AirPlayReceiver::features(profile),
            flags: if profile.is_picture_only() {
                AirPlayStatus::empty()
            } else {
                AirPlayStatus::AUDIO
            },
            deviceid: match profile {
                AirPlayReceiverProfile::CarPlay => format!("{}", device_id),
                _ => format!("{}_atv", device_id),
            },

            vv: match profile {
                AirPlayReceiverProfile::CarPlay => "".into(),
                _ => "2".into(),
            },
            pw: match profile {
                AirPlayReceiverProfile::CarPlay => "".into(),
                _ => "false".into(),
            },
            // ..Default::default()
        }
    }

    // RTSP handlers start

    fn respond_options(&self, _req: &RtspRequest, resp: &mut RtspResponse) {
        resp.set_header(
            HttpHeader::Public,
            "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, POST, GET, PUT, GET_PARAMETER, SET_PARAMETER",
        );
    }

    fn respond_flush(&mut self, _req: &RtspRequest, _resp: &mut RtspResponse) -> RtspResult<()> {
        if self.state != AirPlayReceiverState::PostRecord {
            return Err(HttpStatus::NotAcceptable)?;
        }

        Err(HttpStatus::NotAcceptable.into())
    }

    async fn respond_record(&mut self, _req: &RtspRequest, _resp: &mut RtspResponse) -> RtspResult<()> {
        if self.state != AirPlayReceiverState::PreRecord {
            return Err(HttpStatus::NotAcceptable)?;
        }

        self.state.change(AirPlayReceiverState::PostRecord);
        // Session has formally started
        info!("Session starting as RECORD was requested");
        self.sink.on_record().await
    }

    async fn respond_teardown(&mut self, req: &RtspRequest, _resp: &mut RtspResponse) -> RtspResult<()> {
        let data: TeardownPayload = req.get_plist()?;

        let teardown_session = data.streams.is_empty();
        if teardown_session {
            info!("Session TEARDOWN was requested!");
            let _ = self.close_pending_tx.try_send(RtspError::Teardown);
            return Ok(());
        }

        for stream in data.streams {
            info!("Performing teardown of {:?}", stream.stream_type);

            let mut matched = false;
            match stream.stream_type {
                StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio => {
                    if let Some(mut audio) = self.streams.main_audio.take() {
                        audio.shutdown().await;
                        matched = true;
                    }
                }
                StreamType::AltAudio => {
                    if let Some(mut audio) = self.streams.alt_audio.take() {
                        audio.shutdown().await;
                        matched = true;
                    }
                }
                StreamType::Screen => {
                    if let Some(mut screen) = self.streams.screen.take() {
                        screen.shutdown().await;
                        matched = true;
                    }
                }
                _ => {}
            }

            if !matched {
                warn!("No match for stream type {:?} during teardown", stream.stream_type);
            }
        }

        Ok(())
    }

    async fn respond_info(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        let bon = Self::bonjour(self.profile, self.homekit.clone(), self.mac_addr);

        if let Ok(info) = req.get_plist::<InfoMessage>()
            && info
                .qualifier
                .as_ref()
                .is_some_and(|q| q.iter().any(|v| v == "txtAirPlay"))
        {
            return resp.set_plist(InfoMessageTxtAirPlayResponse {
                txt_airplay: bon.to_rtsp_info_string().into(),
            });
        }

        let mut r = InfoMessageResponse {
            status_flags: AirPlayStatus::AUDIO,
            features: self.features,
            extended_features: if self.profile == AirPlayReceiverProfile::CarPlay {
                vec![ExtendedFeature::EnhancedRequestCarUI, ExtendedFeature::VocoderInfo]
            } else {
                vec![]
            },
            device_id: bon.deviceid,
            firmware_revision: "1.0".into(),
            hardware_revision: "1.0".into(),
            keep_alive_low_power: true,
            manufacturer: "unknown".into(),
            model: if self.profile == AirPlayReceiverProfile::CarPlay {
                "CatPlay".into()
            } else {
                "AppleTV3,2".into()
            },
            name: if self.profile == AirPlayReceiverProfile::CarPlay {
                "CarPlay".into()
            } else {
                "CatPlay AirPlay".into()
            },
            source_version: AIRPLAY_SDK_VERSION.into(),

            ..Default::default()
        };

        r.modes = self.sink.init_modes();

        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
            Self::setup_mirroring_audio_defaults(&mut r);
        } else if !self.profile.is_picture_only() {
            Self::setup_audio_defaults(&mut r);
        }
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        if !self.profile.is_picture_only() {
            Self::setup_audio_defaults(&mut r);
        }

        self.sink.on_info(&mut r).await;
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
            r.features = self.features;
            r.status_flags = AirPlayStatus::AUDIO;
            r.audio_formats.retain(|entry| {
                matches!(
                    entry.stream_type,
                    StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio
                ) && entry.audio_type == Some(AudioType::Media)
                    && entry.audio_output_formats.is_some()
            });
            for entry in &mut r.audio_formats {
                entry.audio_input_formats = None;
            }
            r.audio_latencies.retain(|entry| {
                matches!(
                    entry.stream_type,
                    Some(StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio)
                ) && entry.audio_type == Some(AudioType::Media)
                    && entry.output_latency_micros.is_some()
            });
            r.hevc_info = None;
            r.extended_features.clear();
            r.hid_devices.clear();
            r.hid_languages.clear();
            r.bluetooth_ids.clear();
            r.modes = Default::default();
            r.model = "AppleTV3,2".into();
            r.name = "CatPlay AirPlay Mirror".into();
            self.mirroring_audio_formats = r
                .audio_formats
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.stream_type,
                        StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio
                    ) && entry.audio_type == Some(AudioType::Media)
                })
                .filter_map(|entry| entry.audio_output_formats)
                .fold(AudioFormat::empty(), |formats, next| formats | next);
        }
        if self.profile.is_picture_only() {
            // A general CarPlay sink may have copied car capabilities into /info.
            // Picture-only capabilities remain consistent with Bonjour.
            r.features = self.features;
            r.status_flags = AirPlayStatus::empty();
            r.audio_formats.clear();
            r.audio_latencies.clear();
            r.hevc_info = None;
            r.extended_features.clear();
            r.hid_devices.clear();
            r.hid_languages.clear();
            r.bluetooth_ids.clear();
            r.modes = Default::default();
            r.model = "AppleTV3,2".into();
            r.name = "CatPlay AirPlay".into();
        }
        self.modes = AirPlayModeState::new(&r.modes);

        if self.features.contains(AirPlayFeature::CAR) && Self::CARPLAY_FORCE_HEVC {
            r.hevc_info.replace(HevcInfo::default());
        }
        resp.set_plist(r)
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    fn setup_mirroring_audio_defaults(info: &mut InfoMessageResponse) {
        info.audio_formats.push(AudioFormatStruct {
            audio_input_formats: None,
            audio_output_formats: Some(
                AudioFormat::PCM_44100_STEREO
                    | AudioFormat::PCM_48000_STEREO
                    | AudioFormat::AAC_LC_44100_STEREO
                    | AudioFormat::AAC_LC_48000_STEREO
                    | AudioFormat::AAC_ELD_44100_STEREO
                    | AudioFormat::AAC_ELD_48000_STEREO
                    | AudioFormat::ALAC_44100_16_STEREO
                    | AudioFormat::ALAC_44100_24_STEREO
                    | AudioFormat::ALAC_48000_16_STEREO
                    | AudioFormat::ALAC_48000_24_STEREO,
            ),
            stream_type: StreamType::MainAudio,
            audio_type: Some(AudioType::Media),
        });
        info.audio_latencies.push(AudioLatency {
            stream_type: Some(StreamType::MainAudio),
            audio_type: Some(AudioType::Media),
            input_latency_micros: None,
            output_latency_micros: Some(32_000),
            ..Default::default()
        });
    }

    async fn respond_setup(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        match self.state {
            AirPlayReceiverState::InitialSetup => return self.respond_setup_initial(req, resp).await,
            AirPlayReceiverState::PreRecord | AirPlayReceiverState::PostRecord => {}
            _ => return Err(HttpStatus::BadRequest)?,
        }

        let data: Setup = req.get_plist()?;
        debug!("plist data: {data:?}");

        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile.is_experimental_mirroring() {
            if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
                self.validate_mirroring_streams(&data.streams)?;
            }
        }
        if self.profile.is_picture_only()
            && (data.streams.len() != 1 || !matches!(data.streams.first(), Some(StreamDescription::Screen(_))))
        {
            let has_audio = data
                .streams
                .iter()
                .any(|stream| matches!(stream, StreamDescription::Audio(_) | StreamDescription::AudioLegacy(_)));
            self.sink.on_receiver_diagnostic(if has_audio {
                AirPlayReceiverDiagnostic::AudioUnsupported
            } else {
                AirPlayReceiverDiagnostic::UnsupportedStreamSetup
            });
            return Err(RtspError::ProtocolViolation(
                "picture-only mirroring requires one screen per stream SETUP",
            ));
        }
        let mut streams_resp = SetupResponse::default();
        for stream in data.streams {
            match self.setup_stream(stream).await {
                Ok(sdr) => streams_resp.streams.push(sdr),
                Err(error) => {
                    #[cfg(feature = "experimental_airplay_mirroring")]
                    if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
                        if let Some(mut audio) = self.streams.main_audio.take() {
                            audio.shutdown().await;
                        }
                        if let Some(mut screen) = self.streams.screen.take() {
                            screen.shutdown().await;
                        }
                    }
                    return Err(error);
                }
            }
        }

        resp.set_plist(streams_resp)
    }

    async fn init_iap2(&mut self, _handle: AirPlayReceiverHandleRef) {
        let Some(_sink) = self.sink.open_iap2().await else {
            return;
        };

        if !self.wireless {
            return;
        }

        let (client, drain) = AsyncClient::new(false, CsmRemote::airplay(), _sink);
        self.streams.iap2_drain.replace(drain);
        self.streams.iap2.replace(client);
    }

    async fn flush_iap2(&mut self) -> RtspResult<()> {
        if let Some(handle) = self.streams.handle.as_mut()
            && let Some(drain) = self.streams.iap2_drain.as_mut()
            && let Some(packet) = drain.take()
        {
            debug!("Forwarding iAP2 packet");
            let fut = handle.send_command(Command::IApSendMessage(CommandIApSendMessage {
                data: packet.into_inner().into(),
            }))?;
        }

        Ok(())
    }

    async fn setup_screen(&mut self, stream: StreamDescriptionScreen) -> RtspResult<StreamDescriptionResponse> {
        if self.streams.screen.is_some() {
            error!("Attempted to create video sink without TEARDOWN of previous one!");
            return Err(HttpStatus::BadRequest)?;
        }

        if self.profile.is_experimental_mirroring() && self.streams.media_clock.is_none() {
            return Err(RtspError::ProtocolViolation(
                "picture-only screen SETUP requires completed initial SETUP",
            ));
        }
        info!("Opening screen stream with latency {:?}", stream.latency_ms);

        let latency = Duration::from_millis(stream.latency_ms.unwrap_or(0));
        let mut screen_sink = match self.sink.open_screen(latency).await {
            Ok(sink) => sink,
            Err(err) => {
                error!("Failed to open video sink: {err}");
                return Err(RtspError::Code(HttpStatus::InternalServerError));
            }
        };

        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile.is_h264_only_screen() {
            screen_sink = Box::new(super::picture_only::PictureOnlyScreenSink::new(screen_sink));
        }

        if let Err(err) = screen_sink.init().await {
            error!("Failed to initialize video sink: {err}");
            if self.profile.is_h264_only_screen() {
                screen_sink.shutdown().await;
            }
            return Err(RtspError::Code(HttpStatus::InternalServerError));
        }

        let Some(media_clock) = self.streams.media_clock.as_ref() else {
            return Err(RtspError::Code(HttpStatus::InternalServerError));
        };

        let mut bind_ip = self.bind_ip;
        bind_ip.set_port(0);

        let recv = ScreenReceiverSession::new(
            self.stream_encryption,
            stream.stream_connection_id,
            latency,
            screen_sink,
            media_clock.boxed(),
        );
        #[cfg(feature = "experimental_airplay_mirroring")]
        let recv = if self.profile.is_h264_only_screen() {
            recv.with_max_payload_size(crate::carplay_rx::AIRPLAY_MIRRORING_MAX_SCREEN_PAYLOAD)?
        } else {
            recv
        };

        let (helper, local) = spawn_fork("screen_rx", move || {
            TcpHelper::accept_timeout(bind_ip, Self::TCP_REVERSE_CONN_TIMEOUT, recv)
        })?;
        self.streams.screen.replace(helper);

        Ok(StreamDescriptionResponse::new_screen(local.port()))
    }

    async fn setup_audio(&mut self, mut stream: StreamDescriptionAudio) -> RtspResult<StreamDescriptionResponse> {
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring && stream.audio_type == AudioType::Default {
            stream.audio_type = AudioType::Media;
        }
        if self.profile.is_picture_only() {
            return Err(RtspError::ProtocolViolation("picture-only mirroring does not support audio"));
        }
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring
            && (stream.input
                || !matches!(
                    stream.stream_type,
                    StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio
                ))
        {
            return Err(RtspError::ProtocolViolation("mirroring audio must be output-only main audio"));
        }
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
            self.validate_mirroring_audio(&stream)?;
        }
        let audio_stream = match stream.stream_type {
            StreamType::GeneralAudio | StreamType::MainAudio | StreamType::MainHighAudio => &mut self.streams.main_audio,
            StreamType::AltAudio => &mut self.streams.alt_audio,
            _ => return Err(RtspError::Code(HttpStatus::NotAcceptable)),
        };

        if audio_stream.is_some() {
            error!("Attempted to start audio stream without TEARDOWN of previous one!");
            return Err(RtspError::Code(HttpStatus::NotAcceptable));
        }

        let audio_type = stream.audio_type;
        let mut record_codec = None;
        let mut record_sink = None;

        // Sanity: clamp to 32..2000ms to prevent insane memory allocations
        stream.audio_latency_ms = stream.audio_latency_ms.clamp(32, 2000);
        let latency = Duration::from_millis(stream.audio_latency_ms);

        let codec = match RtpReceiver::init_codec(&stream) {
            Ok(v) => v,
            Err(err) => {
                error!("Failed to init audio codec: {err}");
                return Err(err);
            }
        };

        let sink = match self
            .sink
            .open_audio(
                latency,
                stream.stream_type,
                audio_type,
                stream.audio_format,
                codec.output_type(),
                stream.input,
            )
            .await
        {
            Ok(sink) => sink,
            Err(err) => {
                error!("Failed to open audio sink: {err}");
                return Err(RtspError::Code(HttpStatus::InternalServerError));
            }
        };

        if stream.input {
            warn!("Setting up microphone stream, format {:?}", stream.audio_format);
            let codec = match RtpReceiver::init_codec_record(&stream) {
                Ok(v) => v,
                Err(err) => {
                    error!("Failed to init mic audio codec: {err}");
                    return Err(err);
                }
            };

            record_sink.replace(
                match self
                    .sink
                    .open_microphone(stream.stream_type, audio_type, codec.input_type())
                    .await
                {
                    Err(err) => {
                        error!("Failed to open microphone sink: {err}");
                        return Err(RtspError::Code(HttpStatus::InternalServerError));
                    }
                    Ok(sink) => sink,
                },
            );
            record_codec.replace(codec);
        }

        let receiver = match RtpReceiver::new(
            self.bind_ip,
            self.peer_ip,
            self.stream_encryption,
            &stream,
            codec,
            sink,
            record_codec,
            record_sink,
        ) {
            Ok(v) => v,
            Err(err) => {
                error!("Failed to start RTP receiver: {err}");
                return Err(RtspError::Code(HttpStatus::InternalServerError));
            }
        };

        debug!("Created RtpReceiver");

        let resp = StreamDescriptionResponse::new_audio(
            stream.stream_type,
            stream.stream_connection_id,
            receiver.local_port_rtp,
            receiver.local_port_rtcp,
            false,
            true,
        );
        audio_stream.replace(receiver);

        info!(
            "Opening audio stream with type {:?} latency {:?}",
            stream.stream_type, stream.audio_latency_ms
        );

        Ok(resp)
    }

    async fn setup_stream(&mut self, stream: StreamDescription) -> RtspResult<StreamDescriptionResponse> {
        match stream {
            #[cfg(feature = "experimental_airplay_mirroring")]
            StreamDescription::AudioLegacy(v) if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring => {
                if (!v.is_media && !v.using_screen) || v.stream_type != StreamType::GeneralAudio {
                    return Err(RtspError::ProtocolViolation("unsupported legacy mirroring audio"));
                }
                let mut modern = v.as_modern();
                modern.audio_type = AudioType::Media;
                self.setup_audio(modern).await
            }
            StreamDescription::AudioLegacy(v) => self.setup_audio(v.as_modern()).await,
            StreamDescription::Audio(v) => self.setup_audio(v).await,
            StreamDescription::Screen(v) => self.setup_screen(v).await,
            _ => Err(RtspError::Code(HttpStatus::NotAcceptable)),
        }
    }

    async fn respond_setup_initial(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        let result = self.respond_setup_initial_inner(req, resp).await;
        if let Err(err) = &result {
            if self.profile.is_experimental_mirroring() {
                self.sink
                    .on_receiver_diagnostic(AirPlayReceiverDiagnostic::InitialSetupFailed);
            }
            self.state.change(AirPlayReceiverState::Closed(err.clone()));
            self.shutdown().await;
        }
        result
    }

    async fn respond_setup_initial_inner(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        if self.state != AirPlayReceiverState::InitialSetup {
            return Err(RtspError::ProtocolViolation("initial setup in invalid state"));
        }

        self.stream_encryption = AirPlayStreamEncryption::Unconfigured;

        let data: InitialSetup = req.get_plist()?;
        if !self.profile.is_experimental_mirroring() {
            debug!("Initial Setup: {data:?}");
        }
        if self.profile.is_experimental_mirroring() {
            self.sink
                .on_initial_setup_observation(AirPlayInitialSetupObservation {
                    timing_protocol: match data.timing_protocol.as_deref() {
                        None => AirPlayObservedTimingProtocol::Absent,
                        Some("NTP") => AirPlayObservedTimingProtocol::Ntp,
                        Some("PTP") => AirPlayObservedTimingProtocol::Ptp,
                        _ => AirPlayObservedTimingProtocol::Other,
                    },
                    stream_count: data.streams.as_ref().map_or(0, Vec::len),
                    stream_types: data
                        .streams
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .take(8)
                        .map(|stream| match stream {
                            StreamDescription::AudioLegacy(audio) => audio.stream_type,
                            StreamDescription::Audio(audio) => audio.stream_type,
                            StreamDescription::Screen(screen) => screen.stream_type,
                            _ => StreamType::Invalid,
                        })
                        .collect(),
                    encryption_type_bits: data.et.map(|et| et.bits()),
                    is_screen_mirroring_session: data.is_screen_mirroring_session,
                });
            if self.profile.is_picture_only()
                && data
                    .streams
                    .as_ref()
                    .is_some_and(|streams| !streams.is_empty())
            {
                self.sink
                    .on_receiver_diagnostic(AirPlayReceiverDiagnostic::UnsupportedInitialStreams);
                return Err(RtspError::ProtocolViolation(
                    "picture-only mirroring requires separate initial and stream SETUP",
                ));
            }
            #[cfg(feature = "experimental_airplay_mirroring")]
            if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
                let streams = data.streams.as_deref().unwrap_or_default();
                self.validate_mirroring_streams(streams)?;
            }
            if data
                .timing_protocol
                .as_deref()
                .is_some_and(|protocol| protocol != "NTP")
            {
                self.sink
                    .on_receiver_diagnostic(AirPlayReceiverDiagnostic::UnsupportedTimingProtocol);
                return Err(RtspError::ProtocolViolation("experimental mirroring supports only NTP timing"));
            }
        }

        if let Some(et) = data.et {
            // Fun fact about mirroring: iOS enforces FairPlay, but if you add HomeKit pairing to the mix...
            // ...they suddenly cancel out after initial /fp-setup handshake and /pair-verify
            // and the key for the stream is just the the HomeKit shared secret, like a normal modern CarPlay session :|
            // So you can skip FairPlay key descrambler, but then Apple will add 5s connection penalty before even attempting to connect...
            // which likely is fixable by some better Bonjour meta

            if self.pairing.shared_secret().is_some() {
                return Err(RtspError::ProtocolViolation(
                    "after /pair-verify streams are always encrypted using ChaCha shared secret",
                ));
            }

            if et == AirPlayEncryptionType::NONE {
                self.stream_encryption = AirPlayStreamEncryption::None;
                warn!("Encryption of media streams is disabled");
            } else if let Some(ekey) = data.ekey
                && let Some(eiv) = data.eiv
            {
                let audio_iv: [u8; 16] = eiv
                    .as_ref()
                    .try_into()
                    .map_err(|_| RtspError::ProtocolViolation("expected 16-byte eiv in initial setup"))?;

                match et {
                    AirPlayEncryptionType::MFI_SAPv1 => {
                        // AES key scrambled by /auth-setup MFi session
                        if let Err(err) = self.pairing.decrypt_mfi_sap_key(ekey.as_ref(), &audio_iv) {
                            return Err(RtspError::ProtocolViolationString(format!("failed to decrypt MFi SAP keys: {err}")));
                        }

                        let Some((key, iv)) = self.pairing.aes_keys() else {
                            return Err(RtspError::ProtocolViolation("failed to obtain AES keys after MFi SAP setup"));
                        };
                        info!("Using MFi SAP for media streams encryption");
                        self.stream_encryption = AirPlayStreamEncryption::Aes { key, iv };
                    }
                    AirPlayEncryptionType::FAIRPLAY => {
                        // AES key scrambled by /fp-setup scrambler
                        if let Err(err) = self.pairing.decrypt_fairplay_key(ekey.as_ref(), &audio_iv) {
                            return Err(RtspError::ProtocolViolationString(format!(
                                "Failed to decrypt FAIRPLAY keys: {err}"
                            )));
                        }

                        let Some((key, iv)) = self.pairing.aes_keys() else {
                            return Err(RtspError::ProtocolViolation("failed to obtain AES keys after FAIRPLAY setup"));
                        };
                        self.stream_encryption = AirPlayStreamEncryption::Aes { key, iv };
                        info!("Using FairPlay for media streams encryption");
                    }

                    _ => {}
                }
            }
        }

        if matches!(self.stream_encryption, AirPlayStreamEncryption::Unconfigured)
            && let Some(shared_secret) = self.pairing.shared_secret()
        {
            self.stream_encryption = AirPlayStreamEncryption::ChaCha { shared_secret };
            info!("Using HomeKit ChaCha for media streams encryption");
        }

        if self.profile.is_experimental_mirroring()
            && !matches!(
                self.stream_encryption,
                AirPlayStreamEncryption::Aes { .. } | AirPlayStreamEncryption::ChaCha { .. }
            )
        {
            self.sink
                .on_receiver_diagnostic(AirPlayReceiverDiagnostic::MissingMediaEncryption);
            return Err(RtspError::ProtocolViolation(
                "experimental mirroring requires negotiated media encryption",
            ));
        }

        let mut bind_ip = self.bind_ip;
        bind_ip.set_port(0);

        let mut timing_remote = self.peer_ip;
        timing_remote.set_port(data.timing_port);

        let (timing_client, media_clock) = TimingClient::new();
        let (timing_client, timing_bind, _) = UdpHelper::connect(timing_remote, timing_client)?;

        let mut events = None;
        if self.features.contains(AirPlayFeature::CAR) {
            events.replace(self.do_setup_events().await?);
        }

        let handle = Arc::new(AirPlayReceiverHandleImpl::new_with_profile(
            self.streams.event_rtsp.clone(),
            media_clock.clone(),
            self.task_queue_tx.clone(),
            &self.iface,
            self.wireless,
            self.close_pending_tx.clone(),
            self.profile,
        ));

        self.initialize_sink_under_lock(handle.clone()).await?;

        if self.features.contains(AirPlayFeature::CAR) && self.wireless {
            self.init_iap2(handle.clone()).await;
        }

        let (keep_alive_socket, keep_alive_bind) = UdpHelper::bind(bind_ip, KeepAliveServer::new())?;

        let mut r = InitialSetupResponse {
            event_port: events.unwrap_or(0),
            keep_alive_port: Some(keep_alive_bind.port()),
            streams: None,
            timing_port: timing_bind.port(),
            enabled_features: vec![/*ControllerFeature::H264Level51*/],
        };

        if self.features.contains(AirPlayFeature::CAR) && Self::CARPLAY_FORCE_HEVC {
            r.enabled_features.push(ControllerFeature::Hevc);
        }

        self.streams.keep_alive_socket.replace(keep_alive_socket);
        self.streams.timing_socket.replace(timing_client);
        self.streams.media_clock.replace(media_clock);
        self.streams.handle.replace(handle);

        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.profile == AirPlayReceiverProfile::AppleTVH264Mirroring {
            let mut stream_responses = Vec::new();
            for stream in data.streams.unwrap_or_default() {
                stream_responses.push(self.setup_stream(stream).await?);
            }
            if !stream_responses.is_empty() {
                r.streams = Some(stream_responses);
            }
        }

        resp.set_plist(r)?;
        self.state.change(AirPlayReceiverState::PreRecord);
        Ok(())
    }

    async fn initialize_sink_under_lock(&mut self, handle: AirPlayReceiverHandleRef) -> RtspResult<()> {
        let Some(lock) = self.shared.lock(handle.clone()) else {
            warn!("Rejected incoming AirPlay connection, because session is already active!");
            return Err(RtspError::Code(HttpStatus::NotEnoughBandwidth));
        };
        self.lock.replace(lock);
        // An init implementation can allocate resources and then fail.
        self.sink_had_init = true;
        let result = match self.sink.init(handle) {
            Ok(()) => self.sink.on_initial_setup().await,
            Err(err) => Err(err),
        };
        if let Err(err) = &result {
            self.state.change(AirPlayReceiverState::Closed(err.clone()));
            self.shutdown().await;
        }
        result
    }

    async fn do_setup_events(&mut self) -> RtspResult<u16> {
        let secret = self.pairing.shared_secret();

        let mut bind_ip = self.bind_ip;
        bind_ip.set_port(0);

        let event_session = match secret {
            Some(shared_secret) => EventsClient::new(shared_secret),
            None => EventsClient::unencrypted(),
        };

        let event_rtsp = event_session.rtsp_client();

        let (event_socket, event_bind) = TcpHelper::accept_timeout(bind_ip, Self::TCP_REVERSE_CONN_TIMEOUT, event_session)?;
        self.streams.event_socket.replace(event_socket);
        self.streams.event_rtsp.replace(event_rtsp);

        Ok(event_bind.port())
    }

    async fn respond_command(&mut self, req: &RtspRequest, resp: &mut RtspResponse) -> RtspResult<()> {
        if self.profile.is_experimental_mirroring() {
            return Err(RtspError::ProtocolViolation("AirPlay mirroring does not support CarPlay commands"));
        }
        match Command::deserialize_caching(&req.payload, &mut self.serializer) {
            Err(err) => {
                warn!("Failed to deserialize command: {err:?}");
                let status = match err {
                    CommandError::UnknownCommand(_) => HttpStatus::NotImplemented,
                    CommandError::InvalidPayload(_) => HttpStatus::BadRequest,
                    CommandError::FailedDeserialize(_, _) => HttpStatus::BadRequest,
                };
                resp.status = status;
                resp.headers.clear();
                resp.payload.clear();
                resp.cseq = req.cseq;
                Ok(())
            }
            Ok(v) => {
                warn!("Received command: {v:?}");
                let next = self.handle_command(v).await?;
                *resp = next;
                resp.cseq = req.cseq; // User does not have to know the correct CSeq so we have to correct it
                Ok(())
            }
        }
    }

    async fn handle_command(&mut self, cmd: Command) -> RtspResult<RtspResponse> {
        self.sink.on_command_raw(&cmd).await?;

        match cmd {
            Command::ModesChanged(v) => {
                let _old_modes = self.modes;
                self.modes.feed(&v.0);

                // Call on_modes even if modes are unchanged.
                // Looks like iPhone likes asserting modes again in certain situations
                // to be more resilent to racy OEM implementations.
                warn!("Received updated modes: {:?}", self.modes);
                self.sink.on_modes(&self.modes).await;

                Ok(RtspResponse::new(None, HttpStatus::Ok))
            }
            Command::IApSendMessage(cmd) => {
                if let Some(iap2) = self.streams.iap2.as_mut() {
                    iap2.read_frame_buf(&cmd.data);
                } else {
                    warn!("Ignoring incoming iAP2 data because pipe is inactive");
                }

                Ok(RtspResponse::new(None, HttpStatus::Ok))
            }
            _ => Ok(RtspResponse::new(None, HttpStatus::Ok)),
        }
    }

    fn is_idle(&self) -> bool {
        let s = &self.streams;
        s.main_audio.is_none() && s.alt_audio.is_none() && s.screen.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_hap::HomekitStorageFile;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Calls {
        init: usize,
        setup: usize,
        shutdown: usize,
        audio: usize,
        lock_at_init: bool,
        lock_at_shutdown: bool,
        observations: Vec<AirPlayInitialSetupObservation>,
        diagnostics: Vec<AirPlayReceiverDiagnostic>,
    }

    struct CountingSink {
        calls: Arc<Mutex<Calls>>,
        shared: AirPlayServerShared,
        fail_init: bool,
        fail_setup: bool,
    }

    #[async_trait]
    impl crate::carplay_rx::AirPlayReceiverSink for CountingSink {
        fn init(&mut self, _handle: AirPlayReceiverHandleRef) -> RtspResult<()> {
            let mut calls = self.calls.lock().unwrap();
            calls.init += 1;
            calls.lock_at_init = self.shared.borrow().is_some();
            if self.fail_init { Err(RtspError::NotSupported) } else { Ok(()) }
        }

        async fn on_initial_setup(&mut self) -> RtspResult<()> {
            self.calls.lock().unwrap().setup += 1;
            if self.fail_setup { Err(RtspError::NotSupported) } else { Ok(()) }
        }

        async fn on_info(&mut self, info: &mut InfoMessageResponse) {
            // Model a generic bridge that copies all car capabilities into /info.
            info.features = AirPlayReceiver::features(AirPlayReceiverProfile::CarPlay);
            info.model = "CarPlay".into();
            info.hevc_info = Some(HevcInfo::default());
            AirPlayReceiver::setup_audio_defaults(info);
        }

        fn on_initial_setup_observation(&mut self, observation: AirPlayInitialSetupObservation) {
            self.calls.lock().unwrap().observations.push(observation);
        }

        fn on_receiver_diagnostic(&mut self, diagnostic: AirPlayReceiverDiagnostic) {
            self.calls.lock().unwrap().diagnostics.push(diagnostic);
        }

        async fn open_audio(
            &mut self,
            _latency: Duration,
            _stream_type: StreamType,
            _audio_type: crate::msg::AudioType,
            _audio_format: crate::msg::AudioFormat,
            _pcm_format: crate::audio::AudioStreamBasicDescription,
            _duplex: bool,
        ) -> RtspResult<crate::audio::AudioPlayerBox<i16>> {
            self.calls.lock().unwrap().audio += 1;
            Err(RtspError::NotSupported)
        }
    }

    impl EventReconciler for CountingSink {
        type Error = RtspError;
    }
    impl EventSleeper for CountingSink {}
    impl AsyncShutdown for CountingSink {
        async fn shutdown(&mut self) {
            let mut calls = self.calls.lock().unwrap();
            calls.shutdown += 1;
            calls.lock_at_shutdown = self.shared.borrow().is_some();
        }
    }

    fn receiver(
        profile: AirPlayReceiverProfile,
        shared: AirPlayServerShared,
        fail_init: bool,
        fail_setup: bool,
    ) -> (AirPlayReceiver, Arc<Mutex<Calls>>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let sink = CountingSink {
            calls: calls.clone(),
            shared: shared.clone(),
            fail_init,
            fail_setup,
        };
        (
            AirPlayReceiver::new(
                "lo",
                MacAddr6::new(2, 0, 0, 0, 0, 1),
                HomekitStorageFile::memory(),
                None,
                shared,
                Box::new(sink),
                profile,
            ),
            calls,
        )
    }

    fn handle(receiver: &AirPlayReceiver) -> AirPlayReceiverHandleRef {
        Arc::new(AirPlayReceiverHandleImpl::new_with_profile(
            None,
            TimingClient::new().1,
            receiver.task_queue_tx.clone(),
            "lo",
            true,
            receiver.close_pending_tx.clone(),
            receiver.profile,
        ))
    }

    #[tokio::test]
    async fn admission_holds_lock_during_init_and_shutdown_and_is_idempotent() {
        let shared = AirPlayServerShared::new();
        let (mut receiver, calls) = receiver(AirPlayReceiverProfile::AppleTV, shared.clone(), false, false);
        receiver
            .initialize_sink_under_lock(handle(&receiver))
            .await
            .unwrap();
        assert!(calls.lock().unwrap().lock_at_init);
        assert!(shared.borrow().is_some());
        receiver.shutdown().await;
        receiver.shutdown().await;
        let calls = calls.lock().unwrap();
        assert_eq!((calls.init, calls.setup, calls.shutdown), (1, 1, 1));
        assert!(calls.lock_at_shutdown);
        assert!(shared.borrow().is_none());
    }

    #[tokio::test]
    async fn competing_connection_does_not_initialize_or_shutdown_sink() {
        let shared = AirPlayServerShared::new();
        let (mut first, _) = receiver(AirPlayReceiverProfile::AppleTV, shared.clone(), false, false);
        first
            .initialize_sink_under_lock(handle(&first))
            .await
            .unwrap();
        let owner = shared.borrow().unwrap();
        let (mut rejected, calls) = receiver(AirPlayReceiverProfile::AppleTV, shared.clone(), false, false);
        assert!(
            rejected
                .initialize_sink_under_lock(handle(&rejected))
                .await
                .is_err()
        );
        rejected.shutdown().await;
        drop(rejected);
        assert!(Arc::ptr_eq(&owner, &shared.borrow().unwrap()));
        assert_eq!(calls.lock().unwrap().init, 0);
        assert_eq!(calls.lock().unwrap().shutdown, 0);
        first.shutdown().await;
    }

    #[tokio::test]
    async fn failed_init_and_failed_setup_clean_partial_sink_before_unlocking() {
        for (fail_init, fail_setup) in [(true, false), (false, true)] {
            let shared = AirPlayServerShared::new();
            let (mut receiver, calls) = receiver(AirPlayReceiverProfile::AppleTV, shared.clone(), fail_init, fail_setup);
            assert!(
                receiver
                    .initialize_sink_under_lock(handle(&receiver))
                    .await
                    .is_err()
            );
            assert!(matches!(receiver.state, AirPlayReceiverState::Closed(_)));
            assert!(shared.borrow().is_none());
            receiver.shutdown().await;
            let calls = calls.lock().unwrap();
            assert_eq!(calls.init, 1);
            assert_eq!(calls.setup, usize::from(!fail_init));
            assert_eq!(calls.shutdown, 1);
            assert!(calls.lock_at_shutdown);
        }
    }

    #[test]
    fn original_profiles_keep_their_capabilities() {
        let base = AirPlayFeature::AUDIO
            | AirPlayFeature::AUDIO_PCM
            | AirPlayFeature::AUDIO_UNENCRYPTED
            | AirPlayFeature::SCREEN
            | AirPlayFeature::ROTATE
            | AirPlayFeature::UNIFIED_BONJOUR
            | AirPlayFeature::AUDIO_AAC_LC;
        assert_eq!(
            AirPlayReceiver::features(AirPlayReceiverProfile::CarPlay),
            base | AirPlayFeature::AUDIO_AES_128_MFI_SAP_V1
                | AirPlayFeature::CAR
                | AirPlayFeature::CARPLAY_CONTROL
                | AirPlayFeature::HK_PAIRING_AND_ENCRYPT
        );
        assert_eq!(
            AirPlayReceiver::features(AirPlayReceiverProfile::AppleTV),
            base | AirPlayFeature::AUDIO_AAC_ELD
                | AirPlayFeature::AUDIO_ALAC
                | AirPlayFeature::VIDEO_FAIRPLAY
                | AirPlayFeature::AUTHENTICATION4
                | AirPlayFeature::SUPPORTS_SCREEN_MULTI_CODEC
        );
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn pairing_handler_diagnostics_are_allowlisted_and_picture_only() {
        for profile in [AirPlayReceiverProfile::AppleTV, AirPlayReceiverProfile::AppleTVH264VideoOnly] {
            let (mut receiver, calls) = receiver(profile, AirPlayServerShared::new(), false, false);
            let mut response = RtspResponse::new(None, HttpStatus::Ok);
            let invalid = RtspRequest::post("/fp-setup", bytes::BytesMut::new());
            assert!(
                receiver
                    .process_request(&invalid, &mut response)
                    .await
                    .is_err()
            );
            let mut payload = [0u8; 16];
            payload[4] = 3;
            let valid = RtspRequest::post("/fp-setup", bytes::BytesMut::from(&payload[..]));
            receiver
                .process_request(&valid, &mut response)
                .await
                .unwrap();
            let calls = calls.lock().unwrap();
            if profile.is_picture_only() {
                assert_eq!(
                    calls.diagnostics,
                    [
                        AirPlayReceiverDiagnostic::FairPlayHandlerFailed,
                        AirPlayReceiverDiagnostic::FairPlayHandlerCompleted
                    ]
                );
            } else {
                assert!(calls.diagnostics.is_empty());
            }
        }
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn picture_only_info_and_bonjour_refuse_audio_hevc_and_carplay_overrides() {
        let profile = AirPlayReceiverProfile::AppleTVH264VideoOnly;
        let (mut receiver, _) = receiver(profile, AirPlayServerShared::new(), false, false);
        let mut response = RtspResponse::new(None, HttpStatus::Ok);
        receiver
            .respond_info(&RtspRequest::get("/info"), &mut response)
            .await
            .unwrap();
        let info: InfoMessageResponse = response.get_plist().unwrap();
        let bonjour = AirPlayReceiver::bonjour(profile, receiver.homekit.clone(), receiver.mac_addr);
        assert_eq!(info.features, bonjour.features);
        assert_eq!(info.status_flags, AirPlayStatus::empty());
        assert_eq!(bonjour.flags, AirPlayStatus::empty());
        assert!(info.audio_formats.is_empty() && info.audio_latencies.is_empty());
        assert!(info.hevc_info.is_none() && info.extended_features.is_empty());
        assert!(info.modes.resources.is_empty() && info.modes.app_states.is_empty());
        assert!(info.bluetooth_ids.is_empty());
        assert_eq!(info.model, "AppleTV3,2");
        assert!(
            info.features
                .contains(AirPlayFeature::SCREEN | AirPlayFeature::VIDEO_FAIRPLAY)
        );
        assert!(
            !info
                .features
                .intersects(AirPlayFeature::AUDIO | AirPlayFeature::CAR | AirPlayFeature::SUPPORTS_SCREEN_MULTI_CODEC)
        );
        let handle = handle(&receiver);
        assert!(handle.is_mirroring() && !handle.is_car());
        assert!(matches!(
            handle.send_command(Command::ForceKeyFrame(crate::msg::CommandForceKeyFrame {})),
            Err(RtspError::EventsUnsupported)
        ));
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn combined_profile_keeps_audio_output_without_car_controls_or_microphone() {
        let profile = AirPlayReceiverProfile::AppleTVH264Mirroring;
        let (mut receiver, _) = receiver(profile, AirPlayServerShared::new(), false, false);
        let mut response = RtspResponse::new(None, HttpStatus::Ok);
        receiver
            .respond_info(&RtspRequest::get("/info"), &mut response)
            .await
            .unwrap();
        let info: InfoMessageResponse = response.get_plist().unwrap();
        let bonjour = AirPlayReceiver::bonjour(profile, receiver.homekit.clone(), receiver.mac_addr);
        assert_eq!(info.features, bonjour.features);
        assert_eq!(info.status_flags, AirPlayStatus::AUDIO);
        assert_eq!(bonjour.flags, AirPlayStatus::AUDIO);
        assert!(
            info.features
                .contains(AirPlayFeature::AUDIO | AirPlayFeature::AUDIO_PCM | AirPlayFeature::SCREEN)
        );
        assert!(!info.features.intersects(
            AirPlayFeature::CAR | AirPlayFeature::CARPLAY_CONTROL | AirPlayFeature::SUPPORTS_SCREEN_MULTI_CODEC | AirPlayFeature::ROTATE
        ));
        assert!(!info.audio_formats.is_empty());
        assert!(info.audio_formats.iter().all(|format| {
            format.audio_input_formats.is_none() && format.audio_type == Some(AudioType::Media) && format.audio_output_formats.is_some()
        }));
        assert!(
            info.audio_latencies
                .iter()
                .all(|latency| latency.input_latency_micros.is_none())
        );
        assert!(info.hevc_info.is_none() && info.extended_features.is_empty());
        assert!(info.modes.resources.is_empty() && info.modes.app_states.is_empty());
        assert!(receiver.profile.is_h264_only_screen());
        assert!(!receiver.profile.is_picture_only());
        assert_eq!(
            receiver.mirroring_audio_formats,
            AudioFormat::PCM_44100_STEREO | AudioFormat::PCM_48000_STEREO
        );
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn combined_stream_preflight_rejects_input_unsupported_and_duplicate_streams() {
        let (mut receiver, calls) = receiver(
            AirPlayReceiverProfile::AppleTVH264Mirroring,
            AirPlayServerShared::new(),
            false,
            false,
        );
        receiver.state = AirPlayReceiverState::PreRecord;
        receiver.mirroring_audio_formats = AudioFormat::PCM_44100_STEREO | AudioFormat::AAC_LC_44100_STEREO;
        let valid_audio = StreamDescriptionAudio::new(
            1,
            Duration::from_millis(32),
            StreamType::MainAudio,
            AudioFormat::AAC_LC_44100_STEREO,
            AudioType::Media,
            None,
            None,
            false,
            false,
            false,
        );
        let valid_screen = StreamDescriptionScreen::new(2, Duration::from_millis(32), "screen");
        assert!(
            receiver
                .validate_mirroring_streams(&[valid_screen.clone().into(), valid_audio.clone().into()])
                .is_ok()
        );
        let legacy = crate::msg::StreamDescriptionAudioLegacy {
            is_media: true,
            stream_type: StreamType::GeneralAudio,
            audio_format: AudioFormat::AAC_LC_44100_STEREO,
            sr: 44_100,
            latency_min: 11_025,
            ..Default::default()
        };
        assert_eq!(legacy.as_modern().audio_latency_ms, 250);
        assert!(
            receiver
                .validate_mirroring_streams(&[StreamDescription::AudioLegacy(legacy.clone())])
                .is_ok()
        );
        assert!(
            receiver
                .validate_mirroring_streams(&[StreamDescription::AudioLegacy(crate::msg::StreamDescriptionAudioLegacy {
                    stream_type: StreamType::GeneralAudio,
                    audio_format: AudioFormat::AAC_LC_44100_STEREO,
                    sr: 44_100,
                    ..Default::default()
                })])
                .is_err()
        );
        assert!(
            receiver
                .validate_mirroring_streams(&[valid_audio.clone().into(), valid_audio.clone().into()])
                .is_err()
        );
        let input_request = RtspRequest::with_plist(
            crate::rtsp_frame::RtspMethod::Setup,
            "/stream",
            Setup::new(&[StreamDescriptionAudio {
                input: true,
                ..valid_audio.clone()
            }
            .into()]),
        )
        .unwrap();
        assert!(
            receiver
                .respond_setup(&input_request, &mut RtspResponse::new(None, HttpStatus::Ok))
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().audio, 0);
        assert!(
            receiver
                .validate_mirroring_streams(&[
                    valid_audio.clone().into(),
                    StreamDescriptionAudio {
                        input: true,
                        ..valid_audio.clone()
                    }
                    .into()
                ])
                .is_err()
        );
        assert!(
            receiver
                .validate_mirroring_streams(&[StreamDescriptionAudio {
                    audio_format: AudioFormat::ALAC_48000_24_STEREO,
                    ..valid_audio
                }
                .into()])
                .is_err()
        );
        assert!(
            receiver
                .validate_mirroring_streams(&[valid_screen.clone().into(), valid_screen.into()])
                .is_err()
        );
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn combined_initial_setup_rejects_bad_timing_or_input_before_sink_init() {
        for setup in [
            InitialSetup {
                timing_protocol: Some("PTP".into()),
                ..Default::default()
            },
            InitialSetup {
                timing_protocol: Some("NTP".into()),
                et: Some(AirPlayEncryptionType::NONE),
                streams: Some(vec![
                    StreamDescriptionAudio {
                        stream_type: StreamType::MainAudio,
                        audio_type: AudioType::Media,
                        audio_format: AudioFormat::PCM_44100_STEREO,
                        input: true,
                        ..Default::default()
                    }
                    .into(),
                ]),
                ..Default::default()
            },
        ] {
            let shared = AirPlayServerShared::new();
            let (mut receiver, calls) = receiver(AirPlayReceiverProfile::AppleTVH264Mirroring, shared.clone(), false, false);
            receiver.mirroring_audio_formats = AudioFormat::PCM_44100_STEREO;
            let request = RtspRequest::with_plist(crate::rtsp_frame::RtspMethod::Setup, "/initial", setup).unwrap();
            assert!(
                receiver
                    .respond_setup_initial(&request, &mut RtspResponse::new(None, HttpStatus::Ok))
                    .await
                    .is_err()
            );
            let calls = calls.lock().unwrap();
            assert_eq!((calls.init, calls.audio, calls.shutdown), (0, 0, 0));
            assert!(shared.borrow().is_none());
            assert!(matches!(receiver.state, AirPlayReceiverState::Closed(_)));
        }
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn picture_only_audio_and_mixed_stream_setup_fail_before_sink_allocation() {
        let (mut receiver, calls) = receiver(
            AirPlayReceiverProfile::AppleTVH264VideoOnly,
            AirPlayServerShared::new(),
            false,
            false,
        );
        receiver.state = AirPlayReceiverState::PreRecord;
        let audio = StreamDescriptionAudio::default();
        assert!(receiver.setup_audio(audio).await.is_err());
        let setup = Setup::new(&[
            StreamDescriptionScreen::new(1, Duration::ZERO, "test").into(),
            StreamDescriptionAudio::default().into(),
        ]);
        let request = RtspRequest::with_plist(crate::rtsp_frame::RtspMethod::Setup, "/stream", setup).unwrap();
        assert!(
            receiver
                .respond_setup(&request, &mut RtspResponse::new(None, HttpStatus::Ok))
                .await
                .is_err()
        );
        assert!(receiver.streams.screen.is_none());
        assert_eq!(calls.lock().unwrap().audio, 0);
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[tokio::test]
    async fn picture_only_initial_setup_reports_safe_shape_and_rejects_unimplemented_forms() {
        for (setup, diagnostic) in [
            (
                InitialSetup {
                    timing_protocol: Some("PTP".into()),
                    ..Default::default()
                },
                AirPlayReceiverDiagnostic::UnsupportedTimingProtocol,
            ),
            (
                InitialSetup {
                    streams: Some(vec![StreamDescriptionScreen::new(1, Duration::ZERO, "test").into()]),
                    ..Default::default()
                },
                AirPlayReceiverDiagnostic::UnsupportedInitialStreams,
            ),
            (InitialSetup::default(), AirPlayReceiverDiagnostic::MissingMediaEncryption),
            (
                InitialSetup {
                    et: Some(AirPlayEncryptionType::NONE),
                    ..Default::default()
                },
                AirPlayReceiverDiagnostic::MissingMediaEncryption,
            ),
        ] {
            let shared = AirPlayServerShared::new();
            let (mut receiver, calls) = receiver(AirPlayReceiverProfile::AppleTVH264VideoOnly, shared.clone(), false, false);
            let request = RtspRequest::with_plist(crate::rtsp_frame::RtspMethod::Setup, "/initial", setup).unwrap();
            assert!(
                receiver
                    .respond_setup_initial(&request, &mut RtspResponse::new(None, HttpStatus::Ok))
                    .await
                    .is_err()
            );
            let calls = calls.lock().unwrap();
            assert_eq!(calls.observations.len(), 1);
            assert!(calls.diagnostics.contains(&diagnostic));
            assert_eq!((calls.init, calls.shutdown), (0, 0));
            assert!(shared.borrow().is_none());
            assert!(matches!(receiver.state, AirPlayReceiverState::Closed(_)));
        }
    }
}
