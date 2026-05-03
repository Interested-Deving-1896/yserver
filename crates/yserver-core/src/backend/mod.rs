//! Backend abstraction. Currently `HostX11Backend` is the sole impl;
//! Phase 6.3+ will add a KMS backend.

pub mod handles;
pub mod params;
mod trait_def;

#[cfg(test)]
pub mod recording;

pub use handles::{
    AnyHandle, ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle, HandleKind, PictureHandle,
    PixmapHandle, VisualHandle, WindowHandle,
};
pub use params::{
    ArcMode, BgState, CapStyle, ClipState, DrawState, FillRule, FillState, FillStyle, GcFunction,
    JoinStyle, LineStyle, SubwindowMode,
};
pub use trait_def::Backend;
