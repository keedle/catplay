//! Compile and exercise the real C2A adapter lifecycle methods without pulling
//! the framebuffer/FFmpeg app into this focused protocol test binary.
#![allow(dead_code)]

#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/proxy_screen.rs"]
mod production_screen;
#[path = "../../../c2a/catplay_c2a/src/proxy_rpc/tx_adapter.rs"]
mod production_adapter;

mod proxy_rpc {
    pub use crate::production_screen::ScreenProxyOp;
    pub mod proxy_screen {
        pub use crate::production_screen::*;
    }
}
