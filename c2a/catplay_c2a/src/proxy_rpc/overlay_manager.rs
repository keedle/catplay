use super::proxy_screen::ScreenProxyOp;
use crate::{
    proxy_rpc::OverlayPolicyDefault,
    ui::{UiRenderer, UiResult, UiState},
};
use catplay_carplay::{
    carplay_rx::AirPlayReceiverHandleRef,
    carplay_tx::TeardownGuard,
    screen::tx::{ScreenTransmitError, ScreenTransmitProxy, ScreenTransmitSink},
    video::EncodedVideoFrame,
};
use catplay_util::{EventReconciler, EventSleeper, EventToken, deadline, event_select, futures_xordered::FuturesOrdered, mpsc};
use futures::{FutureExt, future::BoxFuture};
use log::{debug, error, info, warn};
use std::{
    mem,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::spawn_blocking;

pub struct OverlayManager {
    width: u32,
    height: u32,
    fps: u32,
    dpi: f32,
    persist_dir: Option<PathBuf>,
    lazy_screen: Option<TeardownGuard<ScreenTransmitProxy>>,

    overlay_render_queue: FuturesOrdered<BoxFuture<'static, UiResult<EncodedVideoFrame>>>,
    overlay_current: Option<UiState>,
    overlay_last_keyframe: Option<EncodedVideoFrame>,

    iphone_peer: Option<AirPlayReceiverHandleRef>,

    iphone_screen_rx: Option<mpsc::UnboundedReceiver<ScreenProxyOp>>,
    iphone_screen_active: bool,

    renderer: Option<Arc<Mutex<UiRenderer>>>,

    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_peer: Option<AirPlayReceiverHandleRef>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_mode: bool,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_screen: Option<super::MirrorBridgeReceiver>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_last_config: Option<catplay_carplay::video::AvccConfigExtended>,
    #[cfg(feature = "experimental_airplay_mirroring")]
    mirror_revision: Option<usize>,
    policy: OverlayPolicyDefault,
    // stream_state: ResourceTransferType,
}

impl OverlayManager {
    pub fn new(width: u32, height: u32, fps: u32, dpi: f32, persist_dir: Option<PathBuf>) -> Self {
        Self {
            width,
            height,
            fps,
            dpi,
            persist_dir,

            overlay_render_queue: Default::default(),
            overlay_current: None,
            overlay_last_keyframe: None,

            iphone_peer: None,

            iphone_screen_rx: None,
            iphone_screen_active: false,

            renderer: None,

            #[cfg(feature = "experimental_airplay_mirroring")]
            mirror_peer: None,
            #[cfg(feature = "experimental_airplay_mirroring")]
            mirror_mode: false,
            #[cfg(feature = "experimental_airplay_mirroring")]
            mirror_screen: None,
            #[cfg(feature = "experimental_airplay_mirroring")]
            mirror_last_config: None,
            #[cfg(feature = "experimental_airplay_mirroring")]
            mirror_revision: None,
            policy: OverlayPolicyDefault::new(),
            lazy_screen: None,
        }
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn enable_mirroring(&mut self) {
        self.mirror_mode = true;
        self.clear_render_queue();
        self.overlay_current = None;
        self.overlay_last_keyframe = None;
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn set_mirror_peer(&mut self, handle: Option<AirPlayReceiverHandleRef>) {
        if let Some(mut screen) = self.mirror_screen.take() {
            screen.close();
        }
        self.mirror_peer = handle;
        self.mirror_last_config = None;
        self.clear_render_queue();
        self.overlay_current = None;
        self.overlay_last_keyframe = None;
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn set_mirror_screen(&mut self, receiver: super::MirrorBridgeReceiver) {
        if let Some(mut old) = self.mirror_screen.take() {
            old.close();
        }
        self.mirror_screen = Some(receiver);
        self.mirror_last_config = None;
        self.mirror_revision = None;
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn has_mirror_screen(&self) -> bool {
        self.mirror_screen
            .as_ref()
            .is_some_and(|screen| !screen.is_closed())
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn fail_mirror_stream(&mut self) {
        if let Some(screen) = self.mirror_screen.as_mut() {
            screen.close();
        }
        if let Some(handle) = self.mirror_peer.as_ref() {
            handle.close(catplay_carplay::rtsp_frame::RtspError::NotSupported);
        }
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn recover_mirror_stream(&mut self) {
        if let Some(receiver) = self.mirror_screen.as_mut() {
            receiver.invalidate_until_idr();
        }
        if let Some(screen) = self.lazy_screen.as_mut() {
            screen.reset_for_keyframe();
        }
        self.mirror_last_config = None;
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    async fn reconcile_mirror(&mut self) {
        let Some(receiver) = self.mirror_screen.as_mut() else {
            return;
        };
        if receiver.recovery_expired() {
            self.fail_mirror_stream();
            return;
        }
        if receiver.is_closed() {
            // Explicit screen TEARDOWN ends this stream, not the RTSP owner.
            // Socket/protocol failures are escalated by the receiver itself.
            self.mirror_screen.take();
            self.retire_mirror_car_stream();
            return;
        }
        let revision = receiver.revision();
        if self
            .mirror_revision
            .is_some_and(|previous| previous != revision)
        {
            // Reset as soon as config/loss changes, even before a new IDR is
            // available. Already-written TCP bytes cannot be retracted.
            if let Some(screen) = self.lazy_screen.as_mut() {
                screen.reset_for_keyframe();
            }
            self.mirror_last_config = None;
        }
        self.mirror_revision = Some(revision);
        let Some(screen) = self.lazy_screen.as_mut() else {
            return;
        };
        // Do not hand a frame to the legacy queue while it is at its limit.
        // This confines mirror queue memory to a fixed frame count, each gated
        // by the receiver's byte bound, and avoids blocking the car event loop.
        if screen.queue_len() >= screen.queue_max().min(2) {
            return;
        }
        let Some((revision, frame)) = receiver.take_frame_tagged() else {
            return;
        };
        if !receiver.is_revision_current(revision) {
            return;
        }
        if frame.config.is_some() && self.mirror_last_config.is_some() && self.mirror_last_config != frame.config {
            // The bridge supplied a newly validated IDR and config. Discard
            // stale queued output while preserving TCP/crypto state, then send
            // Configure immediately before this first frame of the new stream.
            screen.reset_for_keyframe();
            self.mirror_last_config = None;
        }
        if let Some(config) = frame.config.as_ref() {
            self.mirror_last_config = Some(config.clone());
        }
        let result = tokio::time::timeout(Duration::from_millis(100), screen.push_frame(frame)).await;
        if !matches!(result, Ok(Ok(()))) {
            self.recover_mirror_stream();
        }
    }

    /// Retire downstream state from a previous owner without failing the
    /// current phone, which may already be a newly admitted successor.
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub fn retire_mirror_car_stream(&mut self) {
        self.mirror_last_config = None;
        if let Some(screen) = self.lazy_screen.as_mut() {
            screen.reset_for_keyframe();
        }
        self.lazy_screen.take();
    }

    pub fn set_car_peer(&mut self, lazy_screen: TeardownGuard<ScreenTransmitProxy>) {
        self.lazy_screen.replace(lazy_screen);
    }

    pub fn drop_car_peer(&mut self) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        #[cfg(feature = "experimental_airplay_mirroring")]
        {
            self.mirror_last_config = None;
            if self.lazy_screen.is_some() {
                self.fail_mirror_stream();
            }
        }
        self.lazy_screen.take()
    }

    pub fn has_car_peer(&mut self) -> bool {
        self.lazy_screen.is_some()
    }

    pub fn set_iphone_peer(&mut self, iphone: Option<AirPlayReceiverHandleRef>) {
        self.iphone_peer = iphone;
    }

    pub fn set_iphone_peer_screen(&mut self, screen: Option<mpsc::UnboundedReceiver<ScreenProxyOp>>) {
        self.iphone_screen_rx = screen;
        self.iphone_screen_active = false;
        if let Some(peer) = self.iphone_peer.as_mut() {
            peer.request_keyframe();
        }
    }

    pub fn on_hid_interact(&mut self) -> bool {
        self.policy.on_hid_interact()
    }

    fn clear_render_queue(&mut self) {
        std::mem::take(&mut self.overlay_render_queue);
        self.release_renderer();
    }

    fn reset_overlay(&mut self) {
        self.overlay_current.take();
        self.overlay_last_keyframe.take();

        if let Some(iphone) = self.iphone_peer.as_mut() {
            iphone.request_keyframe();
        }
    }

    async fn reconcile_screen_iphone(&mut self) {
        let Some(op) = self.iphone_screen_rx.as_mut().and_then(|rx| rx.take()) else {
            return;
        };

        match op {
            ScreenProxyOp::IphoneFrame { frame, handle, consumed } => {
                if self.overlay_current.is_some() {
                    debug!("Dropping iPhone frame because overlay is active");
                    let _ = consumed.send(());
                    return;
                }

                let Some(lazy_screen) = self.lazy_screen.as_mut() else {
                    warn!("Dropping frame, no screen");
                    let _ = consumed.send(());
                    return;
                };

                let pts = frame.pts;
                match lazy_screen.push_frame(frame).await {
                    Err((ScreenTransmitError::NeedsKeyframe, _frame)) => {
                        warn!("Dropping iPhone delta frame, requesting a keyframe ASAP");
                        handle.request_keyframe();
                    }
                    Err((err, _frame)) => {
                        warn!("Sink rejected iPhone frame?: {err}");
                    }
                    Ok(()) => {
                        debug!("Proxied iPhone frame at pts = {}", pts);
                    }
                }
                let _ = consumed.send(());
            }
            ScreenProxyOp::IphoneStreamStart { .. } => self.iphone_screen_active = true,
            ScreenProxyOp::IphoneStreamFinish => self.iphone_screen_active = false,
            _ => {}
        }
    }

    async fn reconcile_overlay_frame(&mut self) {
        // If last overlay was rendered before SETUP, unglitch by sending a copy of the overlay keyframe
        // Check if we have a FIFO rendered frame to publish
        let Some(lazy_screen) = self.lazy_screen.as_mut() else {
            return;
        };

        if self.overlay_current.is_some()
            && let Some(mut keyframe) = self.overlay_last_keyframe.clone()
            && lazy_screen.needs_keyframe()
        {
            warn!("Repeating overlay keyframe");
            keyframe.pts = (Instant::now() + Duration::from_millis(75)).into();

            if let Err((err, _)) = lazy_screen.push_frame(keyframe).await {
                warn!("Sink rejected UI frame?: {err}");
                // TODO: critical
            }
        }

        // TODO dont use push_frame.await; queue instead
        match self.overlay_render_queue.take() {
            Some(Ok(mut frame)) => match self.overlay_current.is_some() {
                false => {
                    warn!("Dropped rendered overlay frame, because overlay is already disabled by now");
                }
                true => {
                    frame.pts = (Instant::now() + Duration::from_millis(75)).into();
                    self.overlay_last_keyframe.replace(frame.clone());

                    if let Err((err, _)) = lazy_screen.push_frame(frame).await {
                        warn!("Sink rejected UI frame?: {err}");
                        // TODO: critical
                    }
                }
            },
            Some(Err(err)) => error!("Failed to render overlay frame: {err}"),
            _ => {}
        }
    }

    async fn reconcile_overlay_next(&mut self) {
        self.policy.set_peer_state(self.peer_state());
        let _ = self.policy.reconcile().await;

        let overlay_next = self.policy.overlay();

        debug!("reconcile_overlay_next");
        match (self.overlay_current.clone(), overlay_next.clone()) {
            (None, None) => {
                // Overlay stays inactive
            }
            (Some(_), None) => {
                // Deactivate overlay and request iPhone keyframe
                self.clear_render_queue();
                self.reset_overlay();
            }
            (Some(old), Some(current)) if old == current => {
                // Overlay stays the same.
                // Consider repeating the keyframe or ticking the UI animation if needed.
                // self.overlay_current.unwrap().tick();
            }
            (_, Some(next)) => {
                // Something new to render
                self.push_overlay_queue(next);
            }
        }

        self.overlay_current = overlay_next.clone();
    }

    fn peer_state(&self) -> OverlayPeerState {
        match (self.iphone_peer.is_some(), self.iphone_screen_active) {
            (false, _) => OverlayPeerState::Unconnected,
            (true, false) => OverlayPeerState::ConnectedWithoutScreen,
            (true, true) => OverlayPeerState::Connected,
        }
    }

    fn push_overlay_queue(&mut self, next: UiState) {
        let renderer = self.init_renderer();
        let latency = Duration::from_millis(25);

        let task = async move {
            spawn_blocking(move || {
                let mut renderer = renderer.lock().unwrap();
                renderer.render_and_encode_frame(next, latency, false)
            })
            .await
            .unwrap()
        };

        if self.overlay_render_queue.len() >= 5 {
            error!("Overlay render queue overflowing, system too slow?");
            let _ = mem::take(&mut self.overlay_render_queue);
        }

        self.overlay_render_queue.push_back(task.boxed());
    }

    pub fn init_renderer(&mut self) -> Arc<Mutex<UiRenderer>> {
        if self.renderer.is_none() {
            let renderer = UiRenderer::new(self.width as _, self.height as _, self.fps, self.dpi, self.persist_dir.clone())
                .expect("failed to init renderer");
            self.renderer.replace(Arc::new(Mutex::new(renderer)));
            info!("Initialized UI renderer");
        }

        self.renderer.clone().unwrap()
    }

    pub fn release_renderer(&mut self) {
        if self.renderer.take().is_some() {
            info!("Released UI renderer");
        }
    }

    pub async fn reconcile(&mut self) {
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.mirror_mode {
            self.reconcile_mirror().await;
            return;
        }
        self.reconcile_overlay_frame().await;
        self.reconcile_overlay_next().await;
        self.reconcile_screen_iphone().await;
    }

    pub fn force_keyframe(&mut self) {
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.mirror_peer.is_some() {
            self.recover_mirror_stream();
            return;
        }
        if let Some(iphone) = self.iphone_peer.as_ref() {
            iphone.request_keyframe();
        } else if let Some(a) = self.lazy_screen.as_mut()
            && self.overlay_current.is_some()
            && let Some(mut last) = self.overlay_last_keyframe.clone()
        {
            warn!("Force keyframe: overlay frame");
            last.pts = (Instant::now() + Duration::from_millis(75)).into();
            let _ = a.push_frame(last.clone()).now_or_never();
        }
    }
}

impl EventSleeper for OverlayManager {
    async fn sleep(&mut self) -> Option<EventToken> {
        #[cfg(feature = "experimental_airplay_mirroring")]
        if self.mirror_mode {
            let can_forward = self
                .lazy_screen
                .as_ref()
                .is_some_and(|screen| screen.queue_len() < screen.queue_max().min(2));
            if can_forward {
                return event_select!(self.mirror_screen, deadline(Instant::now() + Duration::from_millis(20)));
            }
            return deadline(Instant::now() + Duration::from_millis(20))
                .sleep()
                .await;
        }
        event_select!(
            self.policy,
            self.overlay_render_queue,
            self.iphone_screen_rx,
            deadline(Instant::now() + Duration::from_millis(200))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPeerState {
    Unconnected,
    Connected,
    ConnectedWithoutScreen,
}
pub trait OverlayPolicy: EventReconciler<Error = ()> + EventSleeper {
    fn set_peer_state(&mut self, state: OverlayPeerState);

    fn overlay(&mut self) -> Option<UiState>;

    fn on_hid_interact(&mut self) -> bool;
}

#[cfg(all(test, feature = "experimental_airplay_mirroring"))]
mod mirror_forwarding_tests {
    use super::*;
    use crate::proxy_rpc::{MirrorBridge, MirrorLimits};
    use bytes::BytesMut;
    use catplay_carplay::{
        carplay_rx::{AirPlayReceiverProfile, sink::AirPlayReceiverHandleImpl},
        clock::TimingClient,
        screen::{rx::ScreenReceiverSink, tx::ScreenTransmitOp},
        video::{AvccConfig, AvccConfigExtended, Pts},
    };
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn handle() -> AirPlayReceiverHandleRef {
        Arc::new(AirPlayReceiverHandleImpl::new_with_profile(
            None,
            TimingClient::new().1,
            mpsc::unbounded().0,
            "lo",
            true,
            mpsc::channel(1).0,
            AirPlayReceiverProfile::AppleTVH264VideoOnly,
        ))
    }
    fn config() -> AvccConfigExtended {
        AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: vec![0, 0, 0, 1, 0x67, 66, 0, 31, 0, 0, 0, 1, 0x68, 0],
            },
            width: 800,
            height: 480,
            video_latency: Duration::ZERO,
            respect_timestamps: true,
        }
    }
    fn frame(idr: bool) -> EncodedVideoFrame {
        EncodedVideoFrame {
            pts: Pts(Instant::now()),
            width: 800,
            height: 480,
            data: BytesMut::from(if idr {
                &b"\0\0\0\x01\x65\x01"[..]
            } else {
                &b"\0\0\0\x01\x41\x01"[..]
            }),
            config: Some(config()),
            nal_offsets: None,
            is_keyframe: Some(false),
            header_buf: BytesMut::new(),
            chacha_tag_buf: BytesMut::new(),
        }
    }
    fn car_screen() -> (
        TeardownGuard<ScreenTransmitProxy>,
        Arc<Mutex<VecDeque<ScreenTransmitOp>>>,
        Arc<AtomicUsize>,
    ) {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let proxy = ScreenTransmitProxy::new(
            Duration::ZERO,
            queue.clone(),
            Arc::new(Mutex::new(false)),
            catplay_util::notify::Notify::new(),
            catplay_util::notify::Notify::new(),
            8,
        );
        let drops = Arc::new(AtomicUsize::new(0));
        let count = drops.clone();
        (
            TeardownGuard::new(proxy, move || {
                count.fetch_add(1, Ordering::SeqCst);
            }),
            queue,
            drops,
        )
    }

    #[tokio::test]
    async fn mirror_forwards_config_idr_then_delta_to_real_transmitter_queue_with_two_frame_cap() {
        let mut overlay = OverlayManager::new(800, 480, 30, 160.0, None);
        overlay.enable_mirroring();
        overlay.set_mirror_peer(Some(handle()));
        let (screen, queue, _) = car_screen();
        overlay.set_car_peer(screen);
        let (mut sink, receiver) = MirrorBridge::new(MirrorLimits::new(800, 480, 66, 31, 1024).unwrap());
        overlay.set_mirror_screen(receiver);
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(true)).await.unwrap();
        overlay.reconcile().await;
        sink.process_frame(frame(false)).await.unwrap();
        overlay.reconcile().await;
        assert_eq!(
            queue
                .lock()
                .unwrap()
                .iter()
                .filter(|op| op.is_frame())
                .count(),
            2
        );
        assert!(matches!(queue.lock().unwrap().front(), Some(ScreenTransmitOp::Configure(_))));
        // The third frame remains in the bounded bridge rather than entering
        // the legacy queue's larger default capacity.
        sink.process_frame(frame(false)).await.unwrap();
        overlay.reconcile().await;
        assert!(overlay.mirror_screen.as_ref().unwrap().has_pending_frame());
        assert_eq!(
            queue
                .lock()
                .unwrap()
                .iter()
                .filter(|op| op.is_frame())
                .count(),
            2
        );
        assert!(overlay.renderer.is_none());
    }

    #[tokio::test]
    async fn compatible_reconfiguration_reuses_tcp_and_queues_config_before_idr() {
        let mut overlay = OverlayManager::new(800, 480, 30, 160.0, None);
        overlay.enable_mirroring();
        overlay.set_mirror_peer(Some(handle()));
        let (screen, queue, drops) = car_screen();
        overlay.set_car_peer(screen);
        let (mut sink, receiver) = MirrorBridge::new(MirrorLimits::new(800, 480, 66, 31, 1024).unwrap());
        overlay.set_mirror_screen(receiver);
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(true)).await.unwrap();
        overlay.reconcile().await;
        let mut next = config();
        next.video_latency = Duration::from_millis(50);
        sink.set_avcc_config(next.clone()).await.unwrap();
        overlay.reconcile().await;
        assert!(
            queue.lock().unwrap().is_empty(),
            "old queued media must leave before the recovery IDR"
        );
        let mut idr = frame(true);
        idr.config = Some(next);
        sink.process_frame(idr).await.unwrap();
        overlay.reconcile().await;
        assert!(!overlay.mirror_screen.as_ref().unwrap().is_closed());
        let ops = queue.lock().unwrap();
        assert!(matches!(ops.front(), Some(ScreenTransmitOp::Configure(_))));
        assert!(matches!(ops.back(), Some(ScreenTransmitOp::Frame(frame)) if frame.is_known_keyframe()));
        assert_eq!(ops.len(), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(overlay.has_car_peer());
    }

    #[tokio::test]
    async fn retiring_late_previous_screen_setup_preserves_successor_phone_and_idr() {
        let mut overlay = OverlayManager::new(800, 480, 30, 160.0, None);
        overlay.enable_mirroring();
        overlay.set_mirror_peer(Some(handle()));
        let (mut successor_sink, successor) = MirrorBridge::new(MirrorLimits::new(800, 480, 66, 31, 1024).unwrap());
        overlay.set_mirror_screen(successor);
        successor_sink.set_avcc_config(config()).await.unwrap();
        successor_sink.process_frame(frame(true)).await.unwrap();
        // An old owner's asynchronous SETUP can complete after the successor
        // is admitted. Retiring that guard must not close the new bridge.
        let (late_old_screen, _, old_drops) = car_screen();
        overlay.set_car_peer(late_old_screen);
        overlay.retire_mirror_car_stream();
        assert_eq!(old_drops.load(Ordering::SeqCst), 1);
        assert!(!overlay.mirror_screen.as_ref().unwrap().is_closed());
        assert!(overlay.mirror_screen.as_ref().unwrap().has_pending_frame());
        let (fresh_screen, queue, _) = car_screen();
        overlay.set_car_peer(fresh_screen);
        overlay.reconcile().await;
        assert_eq!(
            queue
                .lock()
                .unwrap()
                .iter()
                .filter(|op| op.is_frame())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn no_video_packets_still_expire_initial_idr_wait_and_close_owner() {
        let mut overlay = OverlayManager::new(800, 480, 30, 160.0, None);
        overlay.enable_mirroring();
        let (close_tx, mut close_rx) = mpsc::channel(1);
        overlay.set_mirror_peer(Some(Arc::new(AirPlayReceiverHandleImpl::new_with_profile(
            None,
            TimingClient::new().1,
            mpsc::unbounded().0,
            "lo",
            true,
            close_tx,
            AirPlayReceiverProfile::AppleTVH264VideoOnly,
        ))));
        let (_sink, receiver) = MirrorBridge::new(MirrorLimits::new(800, 480, 66, 31, 1024).unwrap());
        overlay.set_mirror_screen(receiver);
        tokio::time::sleep(Duration::from_millis(3050)).await;
        // The event loop has a bounded wake even without a producer callback.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), overlay.sleep())
                .await
                .is_ok()
        );
        overlay.reconcile().await;
        assert!(matches!(close_rx.try_next(), Ok(Some(_))));
        assert!(!overlay.has_mirror_screen());
    }

    #[tokio::test]
    async fn screen_teardown_keeps_rtsp_owner_and_accepts_fresh_screen() {
        let mut overlay = OverlayManager::new(800, 480, 30, 160.0, None);
        overlay.enable_mirroring();
        let (close_tx, mut close_rx) = mpsc::channel(1);
        let phone = Arc::new(AirPlayReceiverHandleImpl::new_with_profile(
            None,
            TimingClient::new().1,
            mpsc::unbounded().0,
            "lo",
            true,
            close_tx,
            AirPlayReceiverProfile::AppleTVH264VideoOnly,
        ));
        overlay.set_mirror_peer(Some(phone));
        let (screen, _, drops) = car_screen();
        overlay.set_car_peer(screen);
        let limits = MirrorLimits::new(800, 480, 66, 31, 1024).unwrap();
        let (mut sink, receiver) = MirrorBridge::new(limits);
        overlay.set_mirror_screen(receiver);
        sink.set_avcc_config(config()).await.unwrap();
        sink.process_frame(frame(true)).await.unwrap();
        overlay.reconcile().await;
        sink.shutdown().await;
        overlay.reconcile().await;
        assert!(!overlay.has_mirror_screen());
        assert!(!overlay.has_car_peer());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(close_rx.try_next().is_err(), "screen TEARDOWN must not close RTSP");

        let (mut fresh_sink, fresh_receiver) = MirrorBridge::new(limits);
        overlay.set_mirror_screen(fresh_receiver);
        let (fresh_screen, queue, _) = car_screen();
        overlay.set_car_peer(fresh_screen);
        fresh_sink.set_avcc_config(config()).await.unwrap();
        fresh_sink.process_frame(frame(true)).await.unwrap();
        overlay.reconcile().await;
        assert!(overlay.has_mirror_screen());
        assert!(matches!(queue.lock().unwrap().front(), Some(ScreenTransmitOp::Configure(_))));
        assert!(matches!(queue.lock().unwrap().back(), Some(ScreenTransmitOp::Frame(_))));
        assert!(close_rx.try_next().is_err());
    }

    #[tokio::test]
    async fn idle_mirror_never_starts_normal_ui_or_blue_tooth_policy() {
        let mut overlay = OverlayManager::new(800, 480, 30, 160.0, None);
        overlay.enable_mirroring();
        overlay.reconcile().await;
        assert!(overlay.renderer.is_none());
        assert!(overlay.overlay_current.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), overlay.sleep())
                .await
                .is_ok()
        );
        assert!(overlay.renderer.is_none());
    }
}
