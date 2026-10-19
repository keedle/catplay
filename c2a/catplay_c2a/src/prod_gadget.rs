use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(feature = "experimental_airplay_mirroring")]
use catplay_carplay::carplay_rx::{AirPlayReceiverProfile, sink::AirPlayServer};
#[cfg(feature = "experimental_airplay_mirroring")]
use catplay_carplay::carplay_tx::AirPlayTransmitter;
use catplay_carplay::{
    carplay_rx::sink::AirPlayServerShared,
    carplay_tx::{AirPlayTransmitterImpl, AirPlayTransmitterSessionError, TeardownGuard},
    rtsp_frame::RtspError,
};
use catplay_carplay_rx_gadget::{CarPlayWirelessGadget, CarPlayWirelessGadgetError, CarPlayWirelessGadgetState};
use catplay_carplay_tx_gadget::client::{CarPlayUsbClientGadget, CarPlayUsbClientGadgetError, CarPlayUsbClientGadgetStatus};
#[cfg(feature = "experimental_airplay_mirroring")]
use catplay_hap::HomekitStorageFile;
use catplay_hap::HomekitStorageRef;
use catplay_iap2_usb::GadgetError;
#[cfg(feature = "experimental_airplay_mirroring")]
use catplay_iap2_usb::NcmHelper;
use catplay_mfi::MfiDeficeRef;
use catplay_util::{
    AsyncShutdown, EventReconciler, EventSleeper, EventToken, Reconcilable, Reconciler, deadline_after, event_select, mpsc,
};
use log::{debug, error, info};
use tokio::sync::Mutex as TokioMutex;

#[cfg(feature = "experimental_airplay_mirroring")]
use crate::proxy_rpc::MirrorRxSession;
use crate::proxy_rpc::{CarManager, CarPlayRxSession, TxAdapter, TxAdapterOp};

pub struct ProdGadgetConfig {
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub mirroring: Option<crate::MirroringOptions>,

    pub homekit_rx: HomekitStorageRef,
    pub homekit_tx: HomekitStorageRef,
    pub mfi: Option<MfiDeficeRef>,

    // Car side UDC. None = autodiscover only UDC, Some = UDC name
    pub udc_tx: Option<String>,
    // Extra UDC for optional USB receive. None = disabled, Some = UDC name
    pub udc_rx: Option<String>,

    pub bt_name: String,
    pub hci: String,
    pub iface: String,
    pub wpa: bool,
    pub ssid: String,
    pub channel: Option<u8>,
    pub passphrase: String,

    pub bt_cache_file: String,
    pub persist_dir: Option<PathBuf>,
}

pub struct ProdGadget {
    cfg: ProdGadgetConfig,

    rx: Option<Reconciler<CarPlayWirelessGadget<CarPlayRxSession>>>,
    // rx_usb: Option<Reconciler<CarPlayUsbGadget<CarPlayRxSession>>>,
    tx: Option<Reconciler<CarPlayUsbClientGadget>>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_server: Option<AirPlayServer>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_shared: Option<AirPlayServerShared>,
    pending_transmitter: Arc<TokioMutex<Option<TeardownGuard<AirPlayTransmitterImpl>>>>,

    last_tx: Arc<Mutex<Option<ProdGadgetSession>>>,
    burst_wakeups: bool,
}

#[derive(Clone, PartialEq, Debug, thiserror::Error)]
pub enum ProdGadgetError {
    #[error("Rx error: {0}")]
    Rx(#[from] CarPlayWirelessGadgetError),
    #[error("Tx error: {0}")]
    Tx(#[from] CarPlayUsbClientGadgetError),

    #[error("Failed to setup usb client, missing udc? {0}")]
    FailedToSetupUsbClient(GadgetError),

    #[error("Transmitter reached eof before pickup: {0}")]
    UnpickedTransmitterEof(#[from] AirPlayTransmitterSessionError),
    #[error("Failed to initialize connected-car manager: {0}")]
    FailedToSetupCarManager(#[from] RtspError),
    #[cfg(feature = "experimental_airplay_mirroring")]
    #[error("Mirror mode requires a connected car with a nonzero first display")]
    MissingMirrorDisplay,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProdGadgetState {
    Initial,
    WaitingForUdc,
    WaitingForUsbTransmitterGadget,
    WaitingForWirelessCarPlayGadget,

    WaitingForCar,
    WaitingForIPhone,

    Running,

    #[cfg(feature = "experimental_airplay_mirroring")]
    WaitingForMirrorCar,
    #[cfg(feature = "experimental_airplay_mirroring")]
    WaitingForMirrorClient,
    #[cfg(feature = "experimental_airplay_mirroring")]
    Mirroring,
}

type LocalResult<T> = Result<T, LocalError>;
type LocalStatus = LocalResult<LocalState>;
type LocalError = ProdGadgetError;
type LocalState = ProdGadgetState;

impl<T> From<LocalError> for LocalResult<T> {
    fn from(value: LocalError) -> Self {
        Err(value)
    }
}

impl From<LocalState> for LocalStatus {
    fn from(value: LocalState) -> Self {
        Ok(value)
    }
}

impl ProdGadget {
    const RESTART_DELAY: Duration = Duration::from_millis(1000);

    pub fn new(cfg: ProdGadgetConfig) -> Reconciler<Self> {
        Reconciler::new(
            Self {
                cfg,
                // rx_usb: None,
                rx: None,
                tx: None,
                #[cfg(feature = "experimental_airplay_mirroring")]
                mirror_server: None,
                #[cfg(feature = "experimental_airplay_mirroring")]
                mirror_shared: None,
                pending_transmitter: Default::default(),
                last_tx: Default::default(),
                burst_wakeups: false,
            },
            Ok(ProdGadgetState::Initial),
        )
    }
}

impl AsyncShutdown for ProdGadget {
    async fn shutdown(&mut self) {
        self.pending_transmitter.lock().await.take();

        #[cfg(feature = "experimental_airplay_mirroring")]
        if let Some(mut server) = self.mirror_server.take() {
            server.shutdown().await;
        }
        #[cfg(feature = "experimental_airplay_mirroring")]
        self.mirror_shared.take();

        if let Some(mut rx) = self.rx.take() {
            rx.shutdown().await;
        }
        // if let Some(mut rx_usb) = self.rx_usb.take() {
        //     rx_usb.shutdown().await;
        // }
        if let Some(mut tx) = self.tx.take() {
            tx.shutdown().await;
        }
    }
}

#[derive(Clone)]
pub struct ProdGadgetSession {
    pub proxy_tx: mpsc::UnboundedSender<TxAdapterOp>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub mirror_display: Option<(u32, u32)>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub mirror_audio_formats: catplay_carplay::msg::AudioFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhoneReceiverMode {
    CarPlay,
    #[cfg(feature = "experimental_airplay_mirroring")]
    Mirror,
}

fn select_phone_receiver_mode(mirroring_enabled: bool) -> PhoneReceiverMode {
    #[cfg(feature = "experimental_airplay_mirroring")]
    if mirroring_enabled {
        return PhoneReceiverMode::Mirror;
    }
    let _ = mirroring_enabled;
    PhoneReceiverMode::CarPlay
}

#[cfg(feature = "experimental_airplay_mirroring")]
fn mirror_target_for_session(
    session: &ProdGadgetSession,
) -> Option<(mpsc::UnboundedSender<TxAdapterOp>, u32, u32, catplay_carplay::msg::AudioFormat)> {
    if session.proxy_tx.is_closed() {
        return None;
    }
    let (width, height) = session.mirror_display?;
    (width > 0 && height > 0).then(|| (session.proxy_tx.clone(), width, height, session.mirror_audio_formats))
}

impl EventSleeper for ProdGadget {
    async fn sleep(&mut self) -> Option<EventToken> {
        let mut pending_transmitter = self.pending_transmitter.lock().await;
        let fallback = if self.burst_wakeups {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };

        event_select!(
            pending_transmitter.as_mut(),
            self.tx,
            // self.rx_usb,
            self.rx,
            deadline_after(fallback)
        )
    }
}

impl Reconcilable for ProdGadget {
    type Output = LocalStatus;

    async fn on_update(&mut self, new: LocalStatus) -> LocalStatus {
        match new {
            Ok(ref ok) => info!("Progressing -> {ok:?}"),
            Err(ref err) => error!("Entered error state: {err:?}"),
        };

        if new.is_err() {
            debug!("Cleaning up due to error state");
            self.pending_transmitter.lock().await.take();
            if let Some(rx) = self.rx.as_mut() {
                rx.child_mut().set_invites_blocked(true)
            };
            #[cfg(feature = "experimental_airplay_mirroring")]
            if self.cfg.mirroring.is_some() {
                if let Some(mut server) = self.mirror_server.take() {
                    server.shutdown().await;
                }
                self.mirror_shared.take();
                self.last_tx.lock().unwrap().take();
            }
        }

        self.burst_wakeups = new.is_err()
            || matches!(
                new.as_ref(),
                Ok(ProdGadgetState::Initial)
                    | Ok(ProdGadgetState::WaitingForUdc)
                    | Ok(ProdGadgetState::WaitingForUsbTransmitterGadget)
                    | Ok(ProdGadgetState::WaitingForWirelessCarPlayGadget)
            )
            || {
                #[cfg(feature = "experimental_airplay_mirroring")]
                {
                    matches!(new.as_ref(), Ok(ProdGadgetState::WaitingForMirrorCar))
                }
                #[cfg(not(feature = "experimental_airplay_mirroring"))]
                {
                    false
                }
            };

        new
    }

    async fn render(&mut self, prev: LocalStatus, update: Instant) -> LocalStatus {
        let _ = self.rx.reconcile().await;
        // let _ = self.rx_usb.reconcile().await;
        let _ = self.tx.reconcile().await;

        let Ok(_status) = prev.as_ref() else {
            let err = prev.err().unwrap();

            if update.elapsed() > Self::RESTART_DELAY {
                return LocalState::Initial.into();
            }

            return err.into();
        };

        #[cfg(feature = "experimental_airplay_mirroring")]
        let mirroring = self.cfg.mirroring.is_some();
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        let mirroring = false;
        let receiver_mode = select_phone_receiver_mode(mirroring);
        let carplay_rx_mode = receiver_mode == PhoneReceiverMode::CarPlay;
        #[cfg(feature = "experimental_airplay_mirroring")]
        let mirror_rx_mode = receiver_mode == PhoneReceiverMode::Mirror;
        #[cfg(not(feature = "experimental_airplay_mirroring"))]
        let mirror_rx_mode = false;

        if carplay_rx_mode && self.rx.is_none() {
            let shared = AirPlayServerShared::new();
            self.rx.replace(CarPlayWirelessGadget::new(
                self.cfg.homekit_rx.clone(),
                self.cfg.mfi.clone(),
                shared.clone(),
                &self.cfg.bt_name,
                &self.cfg.hci,
                &self.cfg.iface,
                &self.cfg.ssid,
                Some(&self.cfg.passphrase),
                self.cfg.wpa,
                self.cfg.channel,
                Some(&self.cfg.bt_cache_file),
                {
                    let car = self.last_tx.clone();
                    move || {
                        let Some(car) = car.lock().unwrap().clone() else {
                            return CarPlayRxSession::reject();
                        };
                        CarPlayRxSession::new(TxAdapter::new(car.proxy_tx.clone()))
                    }
                },
            ));
        }

        if self.tx.is_none() {
            let gadget = CarPlayUsbClientGadget::new(
                "default",
                "aa:bb:cc:dd:ee:ff",
                self.cfg.udc_tx.as_deref(),
                self.cfg.homekit_tx.clone(),
                false,
            )
            .map_err(|err| LocalError::FailedToSetupUsbClient(err))?;
            self.tx.replace(gadget);
        }
        let mut pending_transmitter = self.pending_transmitter.lock().await;

        // Transmitter reached error status after successful handshake, but before being picked up by the receiver.
        if let Err(err) = pending_transmitter.reconcile().await {
            return Err(err)?;
        }

        let tx = self.tx.as_mut().unwrap();

        #[cfg(feature = "experimental_airplay_mirroring")]
        if mirroring
            && self
                .last_tx
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|session| session.proxy_tx.is_closed())
        {
            self.last_tx.lock().unwrap().take();
            if let Some(mut server) = self.mirror_server.take() {
                server.shutdown().await;
            }
            self.mirror_shared.take();
        }

        if pending_transmitter.is_none()
            && let Some(transmitter) = tx.child_mut().pop_transmitter()
        {
            debug!("Got a transmitter");
            // pending_transmitter.replace(transmitter);

            #[cfg(feature = "experimental_airplay_mirroring")]
            if mirror_rx_mode {
                if let Some(mut server) = self.mirror_server.take() {
                    server.shutdown().await;
                }
                self.mirror_shared.take();
                self.last_tx.lock().unwrap().take();
            }

            #[cfg(feature = "experimental_airplay_mirroring")]
            let mirror_display = if mirror_rx_mode {
                transmitter
                    .info_cached()
                    .displays
                    .first()
                    .filter(|display| display.width_pixels > 0 && display.height_pixels > 0)
                    .map(|display| (display.width_pixels, display.height_pixels))
            } else {
                None
            };

            #[cfg(feature = "experimental_airplay_mirroring")]
            if mirror_rx_mode && mirror_display.is_none() {
                return Err(LocalError::MissingMirrorDisplay);
            }

            #[cfg(feature = "experimental_airplay_mirroring")]
            let mirror_audio_formats = crate::proxy_rpc::mirror_car_pcm_formats(transmitter.info_cached());
            let proxy_chan = mpsc::unbounded();
            let car_state = ProdGadgetSession {
                proxy_tx: proxy_chan.0,
                #[cfg(feature = "experimental_airplay_mirroring")]
                mirror_display,
                #[cfg(feature = "experimental_airplay_mirroring")]
                mirror_audio_formats,
            };

            let transmitter2 = CarManager::new(transmitter, proxy_chan.1, self.cfg.persist_dir.clone());
            let transmitter2 = transmitter2
                .await
                .map_err(LocalError::FailedToSetupCarManager)?;
            #[cfg(feature = "experimental_airplay_mirroring")]
            let transmitter2 = if mirror_rx_mode {
                transmitter2.enable_mirroring()
            } else {
                transmitter2
            };
            transmitter2.background();
            self.last_tx.lock().unwrap().replace(car_state);
        }

        if carplay_rx_mode {
            // TODO: this doesn't properly go back to `blocked` after transmit session is closed
            self.rx
                .as_mut()
                .unwrap()
                .child_mut()
                .set_invites_blocked(self.last_tx.lock().unwrap().is_none());
        }

        #[cfg(feature = "experimental_airplay_mirroring")]
        if mirror_rx_mode && self.mirror_server.is_none() {
            let mirror_target = self
                .last_tx
                .lock()
                .unwrap()
                .as_ref()
                .and_then(mirror_target_for_session);
            if let (Some(options), Some((proxy_tx, width, height, audio_formats))) = (self.cfg.mirroring, mirror_target) {
                let iface_ready = NcmHelper::find_mac_address(&self.cfg.iface)
                    .ok()
                    .filter(|_| NcmHelper::is_iface_running(&self.cfg.iface).is_ok_and(|running| running))
                    .filter(|_| NcmHelper::is_mdns_v6_stable(&self.cfg.iface).is_ok());
                if let Some(mac) = iface_ready {
                    const MIRROR_BIND_PORT: u16 = 7001;
                    let shared = AirPlayServerShared::new();
                    let profile = if audio_formats.is_empty() {
                        AirPlayReceiverProfile::AppleTVH264VideoOnly
                    } else {
                        AirPlayReceiverProfile::AppleTVH264Mirroring
                    };
                    let mut server = AirPlayServer::new(
                        MIRROR_BIND_PORT,
                        mac,
                        &self.cfg.iface,
                        HomekitStorageFile::memory(),
                        None,
                        shared.clone(),
                        profile,
                        move || MirrorRxSession::new(proxy_tx.clone(), options, width, height).with_audio_formats(audio_formats),
                    );
                    if let Err(err) = server.bind().await {
                        return Err(LocalError::Rx(CarPlayWirelessGadgetError::AirPlayServer(err.into())));
                    }
                    if let Err(err) = server.start_advertise() {
                        server.shutdown().await;
                        return Err(LocalError::Rx(CarPlayWirelessGadgetError::Bonjour(err.into())));
                    }
                    self.mirror_shared = Some(shared);
                    self.mirror_server = Some(server);
                    info!("Started opt-in picture-only AirPlay listener for live car display {width}x{height}");
                }
            }
        }

        match _status {
            ProdGadgetState::Initial => ProdGadgetState::WaitingForUdc.into(),
            ProdGadgetState::WaitingForUdc => ProdGadgetState::WaitingForUsbTransmitterGadget.into(),
            ProdGadgetState::WaitingForUsbTransmitterGadget => {
                #[cfg(feature = "experimental_airplay_mirroring")]
                if mirroring {
                    return ProdGadgetState::WaitingForMirrorCar.into();
                }
                ProdGadgetState::WaitingForWirelessCarPlayGadget.into()
            }
            ProdGadgetState::WaitingForWirelessCarPlayGadget => ProdGadgetState::WaitingForCar.into(),
            ProdGadgetState::WaitingForCar => {
                #[cfg(feature = "experimental_airplay_mirroring")]
                if mirroring {
                    return ProdGadgetState::WaitingForMirrorCar.into();
                }
                match tx.state() {
                    Ok(CarPlayUsbClientGadgetStatus::Transmitting | CarPlayUsbClientGadgetStatus::TransmitterReadyForPickup) => {
                        ProdGadgetState::WaitingForIPhone.into()
                    }
                    _ if pending_transmitter.is_some() => ProdGadgetState::WaitingForIPhone.into(),
                    _ => ProdGadgetState::WaitingForCar.into(),
                }
            }
            ProdGadgetState::WaitingForIPhone => match self.rx.as_ref().unwrap().state() {
                Ok(CarPlayWirelessGadgetState::Receiving) => ProdGadgetState::Running.into(),
                _ => ProdGadgetState::WaitingForIPhone.into(),
            },
            ProdGadgetState::Running => {
                let still_transmitting = matches!(tx.state(), Ok(CarPlayUsbClientGadgetStatus::Transmitting));
                let still_receiving = matches!(self.rx.as_ref().unwrap().state(), Ok(CarPlayWirelessGadgetState::Receiving));

                match still_receiving && still_transmitting {
                    true => ProdGadgetState::Running.into(),
                    false => ProdGadgetState::WaitingForCar.into(),
                }
            }
            #[cfg(feature = "experimental_airplay_mirroring")]
            ProdGadgetState::WaitingForMirrorCar => {
                let car_ready = matches!(
                    tx.state(),
                    Ok(CarPlayUsbClientGadgetStatus::Transmitting | CarPlayUsbClientGadgetStatus::TransmitterReadyForPickup)
                ) || pending_transmitter.is_some();
                if car_ready && self.mirror_server.is_some() {
                    ProdGadgetState::WaitingForMirrorClient.into()
                } else {
                    ProdGadgetState::WaitingForMirrorCar.into()
                }
            }
            #[cfg(feature = "experimental_airplay_mirroring")]
            ProdGadgetState::WaitingForMirrorClient => {
                let has_car = self
                    .last_tx
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|session| !session.proxy_tx.is_closed());
                let has_mirror_client = self
                    .mirror_shared
                    .as_ref()
                    .and_then(AirPlayServerShared::borrow)
                    .is_some();
                if !has_car {
                    ProdGadgetState::WaitingForMirrorCar.into()
                } else if has_mirror_client {
                    ProdGadgetState::Mirroring.into()
                } else if self.mirror_server.is_some() {
                    ProdGadgetState::WaitingForMirrorClient.into()
                } else {
                    ProdGadgetState::WaitingForMirrorCar.into()
                }
            }
            #[cfg(feature = "experimental_airplay_mirroring")]
            ProdGadgetState::Mirroring => {
                let has_car = self
                    .last_tx
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|session| !session.proxy_tx.is_closed());
                let has_mirror_client = self
                    .mirror_shared
                    .as_ref()
                    .and_then(AirPlayServerShared::borrow)
                    .is_some();
                match (has_car, has_mirror_client) {
                    (true, true) => ProdGadgetState::Mirroring.into(),
                    (true, false) if self.mirror_server.is_some() => ProdGadgetState::WaitingForMirrorClient.into(),
                    _ => ProdGadgetState::WaitingForMirrorCar.into(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_runtime_opt_in_selects_the_existing_carplay_receiver() {
        assert_eq!(select_phone_receiver_mode(false), PhoneReceiverMode::CarPlay);
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[test]
    fn explicit_runtime_opt_in_selects_only_the_mirror_receiver() {
        assert_eq!(select_phone_receiver_mode(true), PhoneReceiverMode::Mirror);
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[test]
    fn mirror_admission_requires_live_car_and_nonzero_display() {
        let (proxy_tx, proxy_rx) = mpsc::unbounded();
        let valid = ProdGadgetSession {
            proxy_tx,
            mirror_display: Some((800, 480)),
            mirror_audio_formats: catplay_carplay::msg::AudioFormat::PCM_48000_STEREO,
        };
        let target = mirror_target_for_session(&valid).unwrap();
        assert_eq!((target.1, target.2), (800, 480));

        let (proxy_tx, _proxy_rx) = mpsc::unbounded();
        let missing = ProdGadgetSession {
            proxy_tx,
            mirror_display: None,
            mirror_audio_formats: catplay_carplay::msg::AudioFormat::PCM_48000_STEREO,
        };
        assert!(mirror_target_for_session(&missing).is_none());

        let (proxy_tx, _proxy_rx) = mpsc::unbounded();
        let zero = ProdGadgetSession {
            proxy_tx,
            mirror_display: Some((800, 0)),
            mirror_audio_formats: catplay_carplay::msg::AudioFormat::PCM_48000_STEREO,
        };
        assert!(mirror_target_for_session(&zero).is_none());

        drop(proxy_rx);
        assert!(mirror_target_for_session(&valid).is_none());
    }
}
