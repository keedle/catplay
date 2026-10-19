//! Host replay of the actual production video framing modules.
//! No copy of the converter is maintained here. This does not exercise RTSP,
//! encryption, the video decoder, audio, USB, or a physical head unit.

#[path = "../../../carplay/catplay_carplay/src/video/nal.rs"]
mod nal;
#[path = "../../../carplay/catplay_carplay/src/video/annexb_iter.rs"]
mod annexb_iter;
#[path = "../../../carplay/catplay_carplay/src/video/annexb_builder.rs"]
mod annexb_builder;
#[path = "../../../carplay/catplay_carplay/src/video/annexb_converter.rs"]
mod annexb_converter;
#[path = "../../../carplay/catplay_carplay/src/video/avcc.rs"]
mod avcc;
#[path = "../../../carplay/catplay_carplay/src/video/avcc_extended.rs"]
mod avcc_extended;
#[path = "../../../carplay/catplay_carplay/src/video/hvcc.rs"]
mod hvcc;

pub mod video {
    pub use crate::nal::*;
    pub use crate::annexb_iter::*;
    pub use crate::annexb_builder::*;
    pub use crate::annexb_converter::*;
    pub use crate::avcc::*;
    pub use crate::avcc_extended::*;
    pub use crate::hvcc::*;
}
