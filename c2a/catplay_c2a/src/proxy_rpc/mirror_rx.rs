//! Picture-only receiver callbacks. Phone control/audio never enter CarPlay RPC.
use crate::{
    MirroringOptions,
    proxy_rpc::{MirrorBridge, MirrorLimits, TxAdapterOp},
};
use async_trait::async_trait;
use catplay_carplay::{
    carplay_rx::AIRPLAY_MIRRORING_MAX_SCREEN_PAYLOAD,
    carplay_rx::{AirPlayReceiverHandleRef, AirPlayReceiverSink},
    modes::ChangeModes,
    msg::{Command, Display, InfoMessageResponse},
    rtsp_frame::{RtspError, RtspResponse, RtspResult},
    screen::rx::ScreenReceiverSinkBox,
};
use catplay_util::{AsyncShutdown, EventReconciler, EventSleeper, mpsc};
use std::{sync::Arc, time::Duration};

/// A retained identity token cannot collide with an old or foreign session.
#[derive(Clone, Debug)]
pub struct MirrorSessionId(Arc<()>);
impl MirrorSessionId {
    pub fn new() -> Self {
        Self(Arc::new(()))
    }
    pub fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

pub struct MirrorRxSession {
    car: mpsc::UnboundedSender<TxAdapterOp>,
    id: MirrorSessionId,
    options: MirroringOptions,
    width: u32,
    height: u32,
    handle: Option<AirPlayReceiverHandleRef>,
    opened_screen: bool,
}
impl MirrorRxSession {
    pub fn new(car: mpsc::UnboundedSender<TxAdapterOp>, options: MirroringOptions, width: u32, height: u32) -> Self {
        Self {
            car,
            id: MirrorSessionId::new(),
            options,
            width,
            height,
            handle: None,
            opened_screen: false,
        }
    }
    fn finish(&mut self) {
        if self.handle.take().is_some() {
            let _ = self
                .car
                .unbounded_send(TxAdapterOp::MirrorDisconnected { id: self.id.clone() });
        }
    }
}
#[async_trait]
impl AirPlayReceiverSink for MirrorRxSession {
    fn init(&mut self, handle: AirPlayReceiverHandleRef) -> RtspResult<()> {
        if self.car.is_closed() || self.width == 0 || self.height == 0 || handle.is_car() || !handle.is_mirroring() || self.handle.is_some()
        {
            return Err(RtspError::NotSupported);
        }
        self.car
            .unbounded_send(TxAdapterOp::MirrorConnected {
                id: self.id.clone(),
                handle: handle.clone(),
            })
            .map_err(|_| RtspError::Closed)?;
        self.handle = Some(handle);
        Ok(())
    }
    fn init_modes(&mut self) -> ChangeModes {
        ChangeModes::default()
    }
    async fn on_initial_setup(&mut self) -> RtspResult<()> {
        if self.handle.is_none() || self.car.is_closed() {
            Err(RtspError::Closed)
        } else {
            Ok(())
        }
    }
    async fn on_info(&mut self, info: &mut InfoMessageResponse) {
        info.displays = vec![Display {
            width_pixels: self.width,
            height_pixels: self.height,
            max_fps: Some(30),
            uuid: "catplay-mirror-car-display".into(),
            ..Default::default()
        }];
    }
    async fn on_command_raw(&mut self, _command: &Command) -> RtspResult<RtspResponse> {
        Err(RtspError::NotSupported)
    }
    async fn open_screen(&mut self, _latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        if self.handle.is_none() || self.car.is_closed() || self.opened_screen {
            return Err(RtspError::NotSupported);
        }
        let limits = MirrorLimits::new(
            self.width,
            self.height,
            self.options.h264_profile_idc,
            self.options.h264_level_idc,
            AIRPLAY_MIRRORING_MAX_SCREEN_PAYLOAD,
        )?;
        let (sink, receiver) = MirrorBridge::new(limits);
        self.car
            .unbounded_send(TxAdapterOp::MirrorScreen {
                id: self.id.clone(),
                receiver,
            })
            .map_err(|_| RtspError::Closed)?;
        self.opened_screen = true;
        Ok(Box::new(sink))
    }
    // Default open_audio/open_microphone/open_iap2 reject or return None.
}
impl EventSleeper for MirrorRxSession {}
impl EventReconciler for MirrorRxSession {
    type Error = RtspError;
}
impl AsyncShutdown for MirrorRxSession {
    async fn shutdown(&mut self) {
        self.finish();
    }
}
impl Drop for MirrorRxSession {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handle(profile: catplay_carplay::carplay_rx::AirPlayReceiverProfile) -> AirPlayReceiverHandleRef {
        Arc::new(catplay_carplay::carplay_rx::sink::AirPlayReceiverHandleImpl::new_with_profile(
            None,
            catplay_carplay::clock::TimingClient::new().1,
            mpsc::unbounded().0,
            "lo",
            true,
            mpsc::channel(1).0,
            profile,
        ))
    }
    fn session(tx: mpsc::UnboundedSender<TxAdapterOp>) -> MirrorRxSession {
        MirrorRxSession::new(
            tx,
            MirroringOptions {
                h264_profile_idc: 66,
                h264_level_idc: 31,
            },
            800,
            480,
        )
    }
    #[tokio::test]
    async fn shutdown_is_tagged_once_and_late_drop_does_not_disconnect_successor() {
        use catplay_carplay::carplay_rx::AirPlayReceiverProfile;
        let (tx, mut rx) = mpsc::unbounded();
        let mut old = session(tx.clone());
        old.init(handle(AirPlayReceiverProfile::AppleTVH264VideoOnly))
            .unwrap();
        let id = match rx.try_next().unwrap().unwrap() {
            TxAdapterOp::MirrorConnected { id, .. } => id,
            _ => panic!("expected mirror admission"),
        };
        old.shutdown().await;
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::MirrorDisconnected { id: released })) if released.matches(&id)));
        let mut next = session(tx);
        next.init(handle(AirPlayReceiverProfile::AppleTVH264VideoOnly))
            .unwrap();
        let successor = match rx.try_next().unwrap().unwrap() {
            TxAdapterOp::MirrorConnected { id, .. } => id,
            _ => panic!("expected successor admission"),
        };
        assert!(!id.matches(&successor));
        old.shutdown().await;
        drop(old);
        assert!(rx.try_next().is_err());
        next.shutdown().await;
        assert!(matches!(rx.try_next(), Ok(Some(TxAdapterOp::MirrorDisconnected { id: released })) if released.matches(&successor)));
    }
    #[tokio::test]
    async fn refuses_carplay_identity_controls_and_uninitialized_screen() {
        use catplay_carplay::{carplay_rx::AirPlayReceiverProfile, msg::CommandForceKeyFrame};
        let (tx, _rx) = mpsc::unbounded();
        let mut sink = session(tx);
        assert!(sink.init(handle(AirPlayReceiverProfile::CarPlay)).is_err());
        assert!(sink.open_screen(Duration::ZERO).await.is_err());
        assert!(
            sink.on_command_raw(&Command::ForceKeyFrame(CommandForceKeyFrame {}))
                .await
                .is_err()
        );
        assert!(sink.open_iap2().await.is_none());
    }
    #[test]
    fn session_tokens_do_not_match_foreign_or_successor_owners() {
        let old = MirrorSessionId::new();
        assert!(old.matches(&old.clone()));
        assert!(!old.matches(&MirrorSessionId::new()));
    }
    #[test]
    fn uninitialized_sink_drop_does_not_disconnect_a_session() {
        let (tx, mut rx) = mpsc::unbounded();
        drop(MirrorRxSession::new(
            tx,
            MirroringOptions {
                h264_profile_idc: 66,
                h264_level_idc: 31,
            },
            800,
            480,
        ));
        assert!(matches!(rx.try_next(), Ok(None)));
    }
}
