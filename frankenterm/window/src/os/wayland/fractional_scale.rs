//! Rendering at the compositor's fractional scale (`wp_fractional_scale_v1`).
//!
//! `wl_surface` only knows integer buffer scales, so on an output scaled to
//! 1.25 the compositor reports 2 and shrinks a doubled buffer, which blurs
//! text. When the compositor offers both `wp_fractional_scale_manager_v1` and
//! `wp_viewporter`, a window instead:
//!
//! * receives its preferred scale as a numerator over 120;
//! * draws a buffer of `round(surface size * scale)` pixels with
//!   `wl_surface.buffer_scale` left at 1;
//! * sets the `wp_viewport` destination to its surface (logical) size, which
//!   maps that buffer back onto the surface.
//!
//! Without either global, or with `enable_wayland_fractional_scale = false`,
//! windows keep the integer `set_buffer_scale` path.

use smithay_client_toolkit::globals::GlobalData;
use wayland_client::globals::GlobalList;
use wayland_client::{Connection as WConnection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    Event as FractionalScaleEvent, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::state::WaylandState;
use super::WaylandConnection;
use crate::Rect;

/// `wp_fractional_scale_v1.preferred_scale` is a numerator over this value.
const PREFERRED_SCALE_DENOMINATOR: f64 = 120.0;

/// Decode a `preferred_scale` event. A zero numerator is not a usable scale.
pub(super) fn scale_from_wire(numerator: u32) -> Option<f64> {
    if numerator == 0 {
        None
    } else {
        Some(f64::from(numerator) / PREFERRED_SCALE_DENOMINATOR)
    }
}

/// Buffer pixels for a surface-local length. The protocol sizes toplevel
/// buffers by rounding halfway away from zero, which is what `f64::round`
/// does.
pub(super) fn surface_to_buffer(surface: i32, scale: f64) -> i32 {
    (f64::from(surface) * scale).round() as i32
}

/// Surface-local length for a buffer length. For scales of 1 or more this
/// inverts [`surface_to_buffer`] exactly.
pub(super) fn buffer_to_surface(pixels: i32, scale: f64) -> i32 {
    (f64::from(pixels) / scale).round() as i32
}

/// The buffer pixel that contains a surface-local pointer coordinate.
pub(super) fn surface_point_to_buffer(coord: f64, scale: f64) -> isize {
    (coord * scale).floor() as isize
}

/// A buffer-pixel rectangle as the smallest surface-local rectangle that
/// covers it: `(x, y, width, height)`.
pub(super) fn buffer_rect_to_surface(rect: Rect, scale: f64) -> (i32, i32, i32, i32) {
    let left = (rect.min_x() as f64 / scale).floor() as i32;
    let top = (rect.min_y() as f64 / scale).floor() as i32;
    let right = (rect.max_x() as f64 / scale).ceil() as i32;
    let bottom = (rect.max_y() as f64 / scale).ceil() as i32;
    (left, top, right - left, bottom - top)
}

/// The two globals fractional scaling needs. Both or neither are kept.
pub(super) struct FractionalScaleGlobals {
    manager: WpFractionalScaleManagerV1,
    viewporter: WpViewporter,
}

impl FractionalScaleGlobals {
    /// Bind both globals, or neither: a lone manager or viewporter is
    /// destroyed again, since one is useless without the other.
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<WaylandState>) -> Option<Self> {
        let manager: Option<WpFractionalScaleManagerV1> = globals.bind(qh, 1..=1, GlobalData).ok();
        let viewporter: Option<WpViewporter> = globals.bind(qh, 1..=1, GlobalData).ok();
        match (manager, viewporter) {
            (Some(manager), Some(viewporter)) => Some(Self {
                manager,
                viewporter,
            }),
            (Some(manager), None) => {
                manager.destroy();
                None
            }
            (None, Some(viewporter)) => {
                viewporter.destroy();
                None
            }
            (None, None) => None,
        }
    }

    /// Create the per-surface objects for the window `window_id`.
    pub(super) fn attach(
        &self,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        qh: &QueueHandle<WaylandState>,
        window_id: usize,
    ) -> FractionalScaleSurface {
        FractionalScaleSurface {
            fractional: self.manager.get_fractional_scale(
                surface,
                qh,
                FractionalScaleData { window_id },
            ),
            viewport: self.viewporter.get_viewport(surface, qh, GlobalData),
            preferred: None,
        }
    }
}

/// Per-window fractional scaling state.
pub(super) struct FractionalScaleSurface {
    fractional: WpFractionalScaleV1,
    viewport: WpViewport,
    /// None until the compositor sends the first `preferred_scale`; until
    /// then the window stays on the integer buffer-scale path.
    preferred: Option<f64>,
}

impl FractionalScaleSurface {
    pub(super) fn preferred(&self) -> Option<f64> {
        self.preferred
    }

    /// Record a new preferred scale. Returns false when it is unchanged.
    pub(super) fn set_preferred(&mut self, scale: f64) -> bool {
        if self.preferred == Some(scale) {
            return false;
        }
        self.preferred = Some(scale);
        true
    }

    /// Map the buffer onto this surface-local size at the next commit.
    pub(super) fn set_destination(&self, surface_width: i32, surface_height: i32) {
        if surface_width > 0 && surface_height > 0 {
            self.viewport.set_destination(surface_width, surface_height);
        }
    }
}

impl Drop for FractionalScaleSurface {
    fn drop(&mut self) {
        self.viewport.destroy();
        self.fractional.destroy();
    }
}

/// User data of a window's `wp_fractional_scale_v1`.
pub(super) struct FractionalScaleData {
    window_id: usize,
}

impl Dispatch<WpFractionalScaleManagerV1, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &WConnection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // wp_fractional_scale_manager_v1 has no events.
    }
}

impl Dispatch<WpViewporter, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as Proxy>::Event,
        _data: &GlobalData,
        _conn: &WConnection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // wp_viewporter has no events.
    }
}

impl Dispatch<WpViewport, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as Proxy>::Event,
        _data: &GlobalData,
        _conn: &WConnection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // wp_viewport has no events.
    }
}

impl Dispatch<WpFractionalScaleV1, FractionalScaleData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: FractionalScaleEvent,
        data: &FractionalScaleData,
        _conn: &WConnection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let FractionalScaleEvent::PreferredScale { scale } = event else {
            return;
        };
        let Some(scale) = scale_from_wire(scale) else {
            log::warn!("Ignoring a Wayland preferred fractional scale of 0");
            return;
        };
        WaylandConnection::with_window_inner(data.window_id, move |inner| {
            inner.set_preferred_fractional_scale(scale);
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        buffer_rect_to_surface, buffer_to_surface, scale_from_wire, surface_point_to_buffer,
        surface_to_buffer,
    };
    use crate::Rect;

    #[test]
    fn preferred_scale_is_a_numerator_over_120() {
        assert_eq!(scale_from_wire(120), Some(1.0));
        assert_eq!(scale_from_wire(150), Some(1.25));
        assert_eq!(scale_from_wire(180), Some(1.5));
        assert_eq!(scale_from_wire(240), Some(2.0));
        assert_eq!(scale_from_wire(0), None);
        let scale = scale_from_wire(132).unwrap();
        assert!((scale - 1.1).abs() < 1e-12);
    }

    #[test]
    fn buffer_size_rounds_half_away_from_zero() {
        // The protocol's own example: 100x50 at 1.5 is a 150x75 buffer.
        assert_eq!(surface_to_buffer(100, 1.5), 150);
        assert_eq!(surface_to_buffer(50, 1.5), 75);
        // 801 * 1.5 = 1201.5 rounds up.
        assert_eq!(surface_to_buffer(801, 1.5), 1202);
        assert_eq!(surface_to_buffer(1745, 1.1), 1920);
        assert_eq!(surface_to_buffer(1000, 1.25), 1250);
    }

    #[test]
    fn surface_size_survives_a_round_trip_through_the_buffer() {
        for numerator in 120..=360 {
            let scale = scale_from_wire(numerator).unwrap();
            for surface in 1..3000 {
                let buffer = surface_to_buffer(surface, scale);
                assert_eq!(
                    buffer_to_surface(buffer, scale),
                    surface,
                    "scale {}/120, surface {}, buffer {}",
                    numerator,
                    surface,
                    buffer
                );
            }
        }
    }

    #[test]
    fn pointer_coordinates_land_in_the_containing_buffer_pixel() {
        assert_eq!(surface_point_to_buffer(0.0, 1.25), 0);
        assert_eq!(surface_point_to_buffer(0.79, 1.25), 0);
        assert_eq!(surface_point_to_buffer(0.9, 1.25), 1);
        assert_eq!(surface_point_to_buffer(10.5, 1.5), 15);
        assert_eq!(surface_point_to_buffer(-0.5, 1.5), -1);
    }

    #[test]
    fn buffer_rect_maps_to_a_covering_surface_rect() {
        let rect = Rect::new(crate::Point::new(15, 31), crate::Size::new(10, 20));
        // x 15..25 -> 12..20 at 1.25; y 31..51 -> 24.8..40.8 -> 24..41.
        assert_eq!(buffer_rect_to_surface(rect, 1.25), (12, 24, 8, 17));
        // At scale 1 the rect is unchanged.
        assert_eq!(buffer_rect_to_surface(rect, 1.0), (15, 31, 10, 20));
    }
}
