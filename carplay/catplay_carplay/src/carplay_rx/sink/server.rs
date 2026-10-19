use std::{io, sync::Arc};

use catplay_bonjour::{Bonjour, BonjourEntryHandle, BonjourType};
use catplay_hap::HomekitStorageRef;
use catplay_mfi::MfiDeficeRef;
use catplay_tokio::TcpServer;
use catplay_util::AsyncShutdown;
use log::debug;
use macaddr::MacAddr6;

use crate::{
    carplay_rx::{
        AirPlayReceiverProfile, AirPlayReceiverSink, AirPlayReceiverSinkCallback,
        sink::{AirPlayReceiver, AirPlayServerShared, CarPlayInviter},
    },
    common::AirPlayBonjourEntry,
    rtsp_session::RtspReceiver,
};

pub struct AirPlayServer {
    port: u16,
    mac_addr: MacAddr6,
    iface: String,
    homekit: HomekitStorageRef,
    mfi: Option<MfiDeficeRef>,

    bonjour_entry: Option<BonjourEntryHandle>,
    bonjour: Option<Bonjour>,
    inviter: Option<CarPlayInviter>,

    listener_handle: Option<TcpServer<RtspReceiver<AirPlayReceiver>>>,
    shared: AirPlayServerShared,
    sink: AirPlayReceiverSinkCallback,
    profile: AirPlayReceiverProfile,
}

impl AirPlayServer {
    pub fn new<T: AirPlayReceiverSink>(
        port: u16,
        mac_addr: MacAddr6,
        iface: &str,
        homekit: HomekitStorageRef,
        mfi: Option<MfiDeficeRef>,
        shared: AirPlayServerShared,
        profile: AirPlayReceiverProfile,
        sink: impl Fn() -> T + Send + Sync + 'static,
    ) -> Self {
        Self {
            port,
            mac_addr,
            iface: iface.into(),
            homekit,
            mfi,
            bonjour_entry: None,
            bonjour: None,
            inviter: None,
            listener_handle: None,
            shared,
            profile,
            sink: Arc::new(move || Box::new((sink)())),
        }
    }

    pub async fn bind(&mut self) -> io::Result<()> {
        debug!("Bind called with addr {}:{}", self.iface, self.port);
        let homekit = self.homekit.clone();
        let mfi = self.mfi.clone();
        let shared = self.shared.clone();

        let callback = self.sink.clone();
        let iface = self.iface.clone();
        let profile = self.profile;
        let mac_addr = self.mac_addr;

        let listener = TcpServer::bind_iface(&self.iface, self.port, move |peer| {
            debug!("Accepted connection from peer {peer:?}");
            let sink = (callback)();

            let session = AirPlayReceiver::new(&iface, mac_addr, homekit.clone(), mfi.clone(), shared.clone(), sink, profile);

            RtspReceiver::new(session)
        })?;
        self.listener_handle.replace(listener);

        Ok(())
    }

    pub fn start_advertise(&mut self) -> io::Result<()> {
        let bonjour = Bonjour::with_iface(&self.iface, BonjourType::Ipv6AndIpv4)?;
        let entry = AirPlayReceiver::bonjour(self.profile, self.homekit.clone(), self.mac_addr);
        let (hostname, instance) = self.advertisement_names();
        let handle = bonjour.register::<AirPlayBonjourEntry>(entry, &hostname, &instance, self.port, &[])?;

        self.bonjour_entry.replace(handle);
        self.bonjour.replace(bonjour);

        debug!("AirPlayServer started advertising");
        Ok(())
    }

    fn advertisement_names(&self) -> (String, String) {
        if self.profile.is_experimental_mirroring() {
            let suffix = self
                .mac_addr
                .to_string()
                .replace(':', "")
                .to_ascii_lowercase();
            let instance = if self.profile.is_picture_only() {
                format!("CatPlay Mirror Probe {suffix}")
            } else {
                format!("CatPlay AirPlay Mirror {suffix}")
            };
            return (format!("catplay-mirror-{suffix}.local."), instance);
        }
        ("carplay.local.".into(), "carplay".into())
    }

    pub fn start_inviting(&mut self) -> bool {
        if !self.profile.is_car() {
            return false;
        }
        let Some(bonjour) = self.bonjour.as_ref() else {
            return false;
        };
        let Some(bonjour_entry) = self.bonjour_entry.as_ref() else {
            return false;
        };

        if self.inviter.is_some() {
            return true;
        }

        self.inviter
            .replace(CarPlayInviter::with_bonjour(bonjour, self.mac_addr, bonjour_entry.clone()));
        true
    }

    pub fn stop_inviting(&mut self) {
        self.inviter = None;
    }
}

impl AsyncShutdown for AirPlayServer {
    async fn shutdown(&mut self) {
        self.stop_inviting();
        if let Some(entry) = self.bonjour_entry.take() {
            entry.close();
        }
        self.bonjour.take();
        if let Some(mut listener) = self.listener_handle.take() {
            // The listener cancellation token is inherited by its accepted
            // connections. Do not close sessions owned by other shared servers.
            listener.shutdown().await;
        }
    }
}

impl Drop for AirPlayServer {
    fn drop(&mut self) {
        debug!("AirPlayServer was dropped!");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp_frame::RtspError;
    use catplay_hap::HomekitStorageFile;
    use catplay_util::{EventReconciler, EventSleeper};

    struct Sink;
    #[async_trait::async_trait]
    impl AirPlayReceiverSink for Sink {}
    impl EventSleeper for Sink {}
    impl EventReconciler for Sink {
        type Error = RtspError;
    }
    impl AsyncShutdown for Sink {}

    fn server(profile: AirPlayReceiverProfile) -> AirPlayServer {
        AirPlayServer::new(
            7001,
            MacAddr6::new(2, 0, 0, 0, 0, 1),
            "lo",
            HomekitStorageFile::memory(),
            None,
            AirPlayServerShared::new(),
            profile,
            || Sink,
        )
    }

    #[test]
    fn legacy_discovery_names_remain_unchanged_and_mirror_does_not_invite() {
        for profile in [AirPlayReceiverProfile::CarPlay, AirPlayReceiverProfile::AppleTV] {
            assert_eq!(server(profile).advertisement_names(), ("carplay.local.".into(), "carplay".into()));
        }
        let mut mirror = server(AirPlayReceiverProfile::AppleTV);
        assert!(!mirror.start_inviting());
        assert!(mirror.inviter.is_none());
    }

    #[cfg(feature = "experimental_airplay_mirroring")]
    #[test]
    fn experimental_discovery_is_distinct_and_never_invites() {
        for (profile, expected_instance) in [
            (AirPlayReceiverProfile::AppleTVH264VideoOnly, "CatPlay Mirror Probe 020000000001"),
            (AirPlayReceiverProfile::AppleTVH264Mirroring, "CatPlay AirPlay Mirror 020000000001"),
        ] {
            let mut mirror = server(profile);
            assert_eq!(
                mirror.advertisement_names(),
                ("catplay-mirror-020000000001.local.".into(), expected_instance.into())
            );
            assert!(!mirror.start_inviting());
            assert!(mirror.inviter.is_none());
        }
    }

    #[tokio::test]
    async fn empty_server_shutdown_is_idempotent() {
        let mut server = server(AirPlayReceiverProfile::CarPlay);
        server.shutdown().await;
        server.shutdown().await;
        assert!(server.listener_handle.is_none());
        assert!(server.bonjour_entry.is_none());
    }
}
