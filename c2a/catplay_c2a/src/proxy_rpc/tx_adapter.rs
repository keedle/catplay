use std::time::Duration;

use catplay_carplay::{
    audio::{AudioPlayerBox, AudioRecorderBox, AudioStreamBasicDescription},
    carplay_rx::AirPlayReceiverHandleRef,
    modes::AirPlayModeState,
    msg::{AudioFormat, AudioType, Command, InfoMessageResponse, StreamType},
    rtsp_frame::{RtspError, RtspResponse, RtspResult},
    screen::rx::ScreenReceiverSinkBox,
};
use catplay_util::{mpsc, oneshot};
use log::{debug, info, warn};

use crate::proxy_rpc::{ScreenProxyOp, proxy_screen::ScreenProxy};

pub struct TxAdapter {
    car: mpsc::UnboundedSender<TxAdapterOp>,
    pending_mic: Option<AudioRecorderBox<i16>>,
    handle: Option<AirPlayReceiverHandleRef>,
}

pub enum TxAdapterOp {
    IphoneConnected {
        handle: AirPlayReceiverHandleRef,
    },
    IphoneDisconnected,
    ScreenChannel {
        channel: mpsc::UnboundedReceiver<ScreenProxyOp>,
    },
    Modes {
        modes: AirPlayModeState,
        consumed: oneshot::Sender<()>,
    },
    Command {
        command: Command,
        response: oneshot::Sender<RtspResponse>,
    },
    PatchInfo {
        info: InfoMessageResponse,
        patched: oneshot::Sender<InfoMessageResponse>,
    },
    SetupAudio {
        latency: Duration,
        stream_type: StreamType,
        audio_type: AudioType,
        audio_format: AudioFormat,
        pcm_format: AudioStreamBasicDescription,
        duplex: bool,

        response: oneshot::Sender<RtspResult<(AudioPlayerBox<i16>, Option<AudioRecorderBox<i16>>)>>,
    },
}

impl TxAdapter {
    pub fn new(car: mpsc::UnboundedSender<TxAdapterOp>) -> Self {
        Self {
            car,
            pending_mic: None,
            handle: None,
        }
    }

    pub async fn proxy_cmd_and_spy(&mut self, command: &Command) -> RtspResult<RtspResponse> {
        // Received over Wi-Fi; blocks RTSP thread
        let os = oneshot::channel();
        let op = TxAdapterOp::Command {
            command: command.clone(),
            response: os.0,
        };
        let _ = self.car.unbounded_send(op);
        os.1.await.map_err(|_| RtspError::Closed)
    }

    pub async fn proxy_audio(
        &mut self,
        latency: Duration,
        stream_type: StreamType,
        audio_type: AudioType,
        audio_format: AudioFormat,
        pcm_format: AudioStreamBasicDescription,
        duplex: bool,
    ) -> RtspResult<AudioPlayerBox<i16>> {
        let os = oneshot::channel();
        let _ = self.car.unbounded_send(TxAdapterOp::SetupAudio {
            latency,
            stream_type,
            audio_type,
            audio_format,
            pcm_format,
            duplex,
            response: os.0,
        });

        let (player, pending_mic) = os.1.await.map_err(|_| RtspError::Closed)??;
        self.pending_mic = pending_mic;
        Ok(player)
    }

    pub async fn proxy_microphone(
        &mut self,
        stream_type: StreamType,
        audio_type: AudioType,
        _pcm_format: AudioStreamBasicDescription,
    ) -> RtspResult<AudioRecorderBox<i16>> {
        match self.pending_mic.take() {
            Some(mic) => Ok(mic),
            None => {
                warn!("Attempted to open microphone stream, but none was pre-initialized! {stream_type:?}, {audio_type:?}");
                Err(RtspError::NotSupported)
            }
        }
    }

    pub async fn proxy_record(&mut self) -> RtspResult<()> {
        info!("iPhone has notified about RECORD (ignored for now)");
        Ok(())
    }

    pub async fn proxy_info(&mut self, info: &mut InfoMessageResponse) {
        let os = oneshot::channel();
        let _ = self.car.unbounded_send(TxAdapterOp::PatchInfo {
            info: info.clone(),
            patched: os.0,
        });
        match os.1.await {
            Err(_) => {
                debug!("Returning unpatched info")
            }
            Ok(v) => {
                *info = v;
            }
        }
    }

    pub async fn proxy_screen(&mut self, _latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        let screen = ScreenProxy::new(self.handle.clone().expect("handle missing"));
        let channel = screen.1;
        let _ = self
            .car
            .unbounded_send(TxAdapterOp::ScreenChannel { channel });
        Ok(Box::new(screen.0))
    }

    pub fn on_init(&mut self, handle: AirPlayReceiverHandleRef) {
        // TODO move to initial setup

        let _ = self
            .car
            .unbounded_send(TxAdapterOp::IphoneConnected { handle: handle.clone() });
        self.handle.replace(handle);
    }

    pub async fn on_shutdown(&mut self) {
        self.on_drop();
    }

    pub fn on_drop(&mut self) {
        // Uninitialized/rejected sinks never owned the phone connection. A sink
        // already shut down must not disconnect a later accepted connection.
        if self.handle.take().is_some() {
            let _ = self.car.unbounded_send(TxAdapterOp::IphoneDisconnected {});
        }
    }

    pub async fn on_modes(&mut self, modes: AirPlayModeState) {
        let os = oneshot::channel();
        let _ = self
            .car
            .unbounded_send(TxAdapterOp::Modes { modes, consumed: os.0 });
        let _ = os.1.await;
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use catplay_carplay::{carplay_rx::sink::AirPlayReceiverHandleImpl, clock::TimingClient};

    fn handle() -> AirPlayReceiverHandleRef {
        let (close_tx, _close_rx) = mpsc::channel(1);
        Arc::new(AirPlayReceiverHandleImpl::new(
            None,
            TimingClient::new().1,
            mpsc::unbounded().0,
            "lo",
            true,
            close_tx,
        ))
    }

    use std::sync::Arc;

    #[test]
    fn dropping_uninitialized_or_rejected_adapter_never_disconnects_owner() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut adapter = TxAdapter::new(tx);
        adapter.on_drop();
        assert!(rx.try_next().is_err());
    }

    #[tokio::test]
    async fn shutdown_disconnects_once_and_late_drop_does_not_disconnect_new_owner() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut old = TxAdapter::new(tx.clone());
        old.on_init(handle());
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::IphoneConnected { .. }))));
        old.on_shutdown().await;
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::IphoneDisconnected))));
        let mut next = TxAdapter::new(tx);
        next.on_init(handle());
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::IphoneConnected { .. }))));
        old.on_drop();
        old.on_shutdown().await;
        assert!(rx.try_next().is_err());
        next.on_drop();
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::IphoneDisconnected))));
        next.on_drop();
        assert!(rx.try_next().is_err());
    }
}
