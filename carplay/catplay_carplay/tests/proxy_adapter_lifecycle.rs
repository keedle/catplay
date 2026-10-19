//! Compile and exercise the real C2A adapter lifecycle methods without pulling
//! the framebuffer/FFmpeg app into this focused protocol test binary.
#![allow(dead_code)]

#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/tx_adapter.rs"]
mod production_adapter;
#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/proxy_screen.rs"]
mod production_screen;

// Compile the actual experimental variants and their lifecycle tests too. The
// adapter's parent module must expose the same types as C2A's proxy_rpc module.
#[cfg(feature = "experimental_airplay_mirroring")]
#[path = "../../../c2a/catplay_c2a/src/runtime_options.rs"]
mod production_runtime_options;
#[cfg(feature = "experimental_airplay_mirroring")]
pub use production_runtime_options::MirroringOptions;
#[cfg(feature = "experimental_airplay_mirroring")]
#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/mirror_bridge.rs"]
mod production_mirror_bridge;
#[cfg(feature = "experimental_airplay_mirroring")]
#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/mirror_rx.rs"]
mod production_mirror_rx;
#[cfg(feature = "experimental_airplay_mirroring")]
use production_mirror_bridge::*;
#[cfg(feature = "experimental_airplay_mirroring")]
use production_mirror_rx::MirrorSessionId;
#[cfg(feature = "experimental_airplay_mirroring")]
#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/mirror_audio.rs"]
mod production_mirror_audio;
#[cfg(feature = "experimental_airplay_mirroring")]
use production_adapter::TxAdapterOp;
#[cfg(feature = "experimental_airplay_mirroring")]
use production_mirror_audio::*;

mod proxy_rpc {
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub use crate::production_adapter::TxAdapterOp;
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub use crate::production_mirror_audio::*;
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub use crate::production_mirror_bridge::*;
    #[cfg(feature = "experimental_airplay_mirroring")]
    pub use crate::production_mirror_rx::MirrorSessionId;
    pub use crate::production_screen::ScreenProxyOp;
    pub mod proxy_screen {
        pub use crate::production_screen::*;
    }
}
