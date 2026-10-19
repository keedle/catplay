use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use catplay_carplay::{
    carplay_rx::{
        AirPlayReceiverHandleRef, AirPlayReceiverProfile, AirPlayReceiverSink,
        sink::{AirPlayReceiver, AirPlayServerShared},
    },
    carplay_tx::{AirPlayTransmitterBootstrap, AirPlayTransmitterImpl},
    msg::InfoMessageResponse,
    rtsp_frame::{RtspError, RtspResult},
    rtsp_session::RtspReceiver,
};
use catplay_hap::HomekitStorageFile;
use catplay_tokio::TcpHelper;
use catplay_util::{AsyncShutdown, EventReconciler, EventSleeper};
use macaddr::MacAddr6;
use tokio::time::timeout;

const OP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct SinkCounts {
    init: AtomicUsize,
    initial_setup: AtomicUsize,
    shutdown: AtomicUsize,
    fail_init: bool,
    fail_initial_setup: bool,
}

struct CountingSink(Arc<SinkCounts>);

#[async_trait]
impl AirPlayReceiverSink for CountingSink {
    fn init(&mut self, _session: AirPlayReceiverHandleRef) -> RtspResult<()> {
        self.0.init.fetch_add(1, Ordering::SeqCst);
        if self.0.fail_init { Err(RtspError::NotSupported) } else { Ok(()) }
    }

    async fn on_initial_setup(&mut self) -> RtspResult<()> {
        self.0.initial_setup.fetch_add(1, Ordering::SeqCst);
        if self.0.fail_initial_setup {
            Err(RtspError::NotSupported)
        } else {
            Ok(())
        }
    }

    async fn on_info(&mut self, _info: &mut InfoMessageResponse) {}
}

impl AsyncShutdown for CountingSink {
    async fn shutdown(&mut self) {
        self.0.shutdown.fetch_add(1, Ordering::SeqCst);
    }
}

impl EventSleeper for CountingSink {}

impl EventReconciler for CountingSink {
    type Error = RtspError;
}

type ReceiverEndpoint = TcpHelper<RtspReceiver<AirPlayReceiver>>;

fn start_receiver(shared: AirPlayServerShared, iface: &'static str, counts: Arc<SinkCounts>) -> (ReceiverEndpoint, SocketAddr) {
    let receiver = AirPlayReceiver::new(
        iface,
        MacAddr6::default(),
        HomekitStorageFile::memory(),
        None,
        shared,
        Box::new(CountingSink(counts)),
        AirPlayReceiverProfile::CarPlay,
    );
    TcpHelper::accept_timeout(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        Duration::ZERO,
        RtspReceiver::new(receiver),
    )
    .expect("start loopback receiver")
}

async fn connect_transmitter(peer_ip: SocketAddr) -> Result<AirPlayTransmitterImpl, ()> {
    timeout(
        OP_TIMEOUT,
        AirPlayTransmitterImpl::connect(AirPlayTransmitterBootstrap {
            homekit: HomekitStorageFile::memory(),
            peer_ip,
            controller_features: vec![],
            remote_homekit_id: None,
        }),
    )
    .await
    .expect("transmitter bootstrap timed out")
    .map_err(|_| ())
}

async fn wait_for_unlock(shared: &AirPlayServerShared) {
    timeout(OP_TIMEOUT, async {
        let mut state = shared.subscribe();
        loop {
            if state.borrow().is_none() {
                return;
            }
            state.changed().await.expect("shared session state closed");
        }
    })
    .await
    .expect("session lock was not released before timeout");
}

fn count(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::SeqCst)
}

#[tokio::test]
async fn rejected_session_does_not_initialize_or_disturb_owner() {
    let shared = AirPlayServerShared::new();
    let owner_counts = Arc::new(SinkCounts::default());
    let contender_counts = Arc::new(SinkCounts::default());
    let next_counts = Arc::new(SinkCounts::default());

    let (_owner_endpoint, owner_addr) = start_receiver(shared.clone(), "owner", owner_counts.clone());
    let (_contender_endpoint, contender_addr) = start_receiver(shared.clone(), "contender", contender_counts.clone());
    let (_next_endpoint, next_addr) = start_receiver(shared.clone(), "next", next_counts.clone());

    let mut owner = connect_transmitter(owner_addr)
        .await
        .expect("owner session should be admitted");
    assert_eq!(count(&owner_counts.init), 1);
    assert_eq!(count(&owner_counts.initial_setup), 1);
    let owner_handle = shared
        .borrow()
        .expect("owner should hold shared session lock");
    assert_eq!(owner_handle.iface(), "owner");

    assert!(
        connect_transmitter(contender_addr).await.is_err(),
        "active session should reject a second owner"
    );
    assert_eq!(count(&contender_counts.init), 0, "rejected sink must not initialize");
    assert_eq!(count(&contender_counts.initial_setup), 0, "rejected sink must not enter setup");
    assert_eq!(
        count(&contender_counts.shutdown),
        0,
        "a sink that was never initialized must not be shut down"
    );
    let current_handle = shared
        .borrow()
        .expect("contender must not release owner lock");
    assert!(Arc::ptr_eq(&owner_handle, &current_handle), "owner handle should remain unchanged");
    assert_eq!(count(&owner_counts.shutdown), 0, "contender must not shut down the active owner");

    timeout(OP_TIMEOUT, owner.shutdown())
        .await
        .expect("owner shutdown timed out");
    wait_for_unlock(&shared).await;
    assert_eq!(count(&owner_counts.shutdown), 1);

    let mut next = connect_transmitter(next_addr)
        .await
        .expect("lock should be reusable after owner shutdown");
    assert_eq!(count(&next_counts.init), 1);
    assert_eq!(shared.borrow().as_deref().map(|handle| handle.iface()), Some("next"));
    timeout(OP_TIMEOUT, next.shutdown())
        .await
        .expect("next session shutdown timed out");
    wait_for_unlock(&shared).await;
}

#[tokio::test]
async fn failed_sink_callbacks_release_lock_and_shutdown_once() {
    let shared = AirPlayServerShared::new();
    let init_failure_counts = Arc::new(SinkCounts {
        fail_init: true,
        ..SinkCounts::default()
    });
    let setup_failure_counts = Arc::new(SinkCounts {
        fail_initial_setup: true,
        ..SinkCounts::default()
    });
    let next_counts = Arc::new(SinkCounts::default());

    let (_init_failure_endpoint, init_failure_addr) = start_receiver(shared.clone(), "init-failure", init_failure_counts.clone());
    let (_setup_failure_endpoint, setup_failure_addr) = start_receiver(shared.clone(), "setup-failure", setup_failure_counts.clone());
    let (_next_endpoint, next_addr) = start_receiver(shared.clone(), "after-failure", next_counts.clone());

    assert!(connect_transmitter(init_failure_addr).await.is_err());
    wait_for_unlock(&shared).await;
    assert_eq!(count(&init_failure_counts.init), 1);
    assert_eq!(count(&init_failure_counts.initial_setup), 0);
    assert_eq!(
        count(&init_failure_counts.shutdown),
        1,
        "partially initialized sink must be shut down once"
    );

    assert!(connect_transmitter(setup_failure_addr).await.is_err());
    wait_for_unlock(&shared).await;
    assert_eq!(count(&setup_failure_counts.init), 1);
    assert_eq!(count(&setup_failure_counts.initial_setup), 1);
    assert_eq!(
        count(&setup_failure_counts.shutdown),
        1,
        "sink whose setup callback failed must be shut down once"
    );

    let mut next = connect_transmitter(next_addr)
        .await
        .expect("failed setup must release admission lock");
    assert_eq!(count(&next_counts.init), 1);
    assert_eq!(shared.borrow().as_deref().map(|handle| handle.iface()), Some("after-failure"));
    timeout(OP_TIMEOUT, next.shutdown())
        .await
        .expect("next session shutdown timed out");
    wait_for_unlock(&shared).await;
}
