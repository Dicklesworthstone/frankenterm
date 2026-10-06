//! The non-macOS stub. Both types are uninhabited: constructors always
//! return [`MetalUnavailable::UnsupportedPlatform`], so every method body is
//! statically unreachable and callers need no `cfg`.

use crate::{
    ClearColor, DeviceCapabilities, FrameError, FrameOutcome, FrameStats, GridExtent,
    MetalUnavailable, SubmissionPath,
};
use raw_window_handle::HasWindowHandle;

/// Uninhabited off macOS; see the crate docs.
#[derive(Debug)]
pub enum MetalDevice {}

impl MetalDevice {
    pub fn system_default() -> Result<Self, MetalUnavailable> {
        Err(MetalUnavailable::UnsupportedPlatform)
    }

    #[must_use]
    pub fn capabilities(&self) -> &DeviceCapabilities {
        match *self {}
    }

    pub fn clear_offscreen(
        &self,
        _width: u32,
        _height: u32,
        _color: ClearColor,
    ) -> Result<Vec<u8>, FrameError> {
        match *self {}
    }
}

/// Uninhabited off macOS; see the crate docs.
#[derive(Debug)]
pub enum MetalRenderer {}

impl MetalRenderer {
    pub fn attach(_window: &impl HasWindowHandle) -> Result<Self, MetalUnavailable> {
        Err(MetalUnavailable::UnsupportedPlatform)
    }

    #[must_use]
    pub fn device(&self) -> &MetalDevice {
        match *self {}
    }

    #[must_use]
    pub fn submission_path(&self) -> SubmissionPath {
        match *self {}
    }

    #[must_use]
    pub fn submission_note(&self) -> Option<&str> {
        match *self {}
    }

    #[must_use]
    pub fn frame_stats(&self) -> FrameStats {
        match *self {}
    }

    pub fn render_clear(
        &self,
        _width: u32,
        _height: u32,
        _grid: GridExtent,
        _color: ClearColor,
    ) -> Result<FrameOutcome, FrameError> {
        match *self {}
    }
}
