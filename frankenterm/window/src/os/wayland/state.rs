use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use smithay_client_toolkit::compositor::{CompositorState, SurfaceData};
use smithay_client_toolkit::data_device_manager::data_device::DataDevice;
use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use smithay_client_toolkit::data_device_manager::DataDeviceManagerState;
use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDevice;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSource;
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::reexports::protocols_wlr::output_management::v1::client::zwlr_output_head_v1::ZwlrOutputHeadV1;
use smithay_client_toolkit::reexports::protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use smithay_client_toolkit::reexports::protocols_wlr::output_management::v1::client::zwlr_output_mode_v1::ZwlrOutputModeV1;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::ThemedPointer;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use smithay_client_toolkit::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_pointer, delegate_primary_selection, delegate_registry, delegate_seat, delegate_shm, delegate_subcompositor, delegate_xdg_shell, delegate_xdg_window, registry_handlers
};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::{delegate_dispatch, Connection, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

use crate::x11::KeyboardWithFallback;

use super::inputhandler::{TextInputData, TextInputState};
use super::pointer::{PendingMouse, PointerUserData};
use super::{OutputManagerData, OutputManagerState, SurfaceUserData, WaylandWindowInner};

// We can't combine WaylandState and WaylandConnection together because
// the run_message_loop has &self(WaylandConnection) and needs to update WaylandState as mut
pub(super) struct WaylandState {
    registry: RegistryState,
    pub(super) output: OutputState,
    pub(super) compositor: CompositorState,
    pub(super) subcompositor: Arc<SubcompositorState>,
    pub(super) text_input: Option<TextInputState>,
    pub(super) output_manager: Option<OutputManagerState>,
    pub(super) seat: SeatState,
    pub(super) xdg: XdgShell,
    pub(super) windows: RefCell<HashMap<usize, Rc<RefCell<WaylandWindowInner>>>>,

    pub(super) active_surface_id: RefCell<Option<ObjectId>>,
    pub(super) last_serial: RefCell<u32>,
    pub(super) keyboard: Option<WlKeyboard>,
    pub(super) keyboard_mapper: Option<KeyboardWithFallback>,
    pub(super) key_repeat_delay: i32,
    pub(super) key_repeat_rate: i32,
    pub(super) keyboard_window_id: Option<usize>,

    pub(super) pointer: Option<ThemedPointer<PointerUserData>>,
    pub(super) surface_to_pending: HashMap<ObjectId, Arc<Mutex<PendingMouse>>>,

    pub(super) data_device_manager_state: DataDeviceManagerState,
    pub(super) data_device: Option<DataDevice>,
    pub(super) copy_paste_source: Option<(CopyPasteSource, String)>,
    pub(super) primary_selection_manager: Option<PrimarySelectionManagerState>,
    pub(super) primary_selection_device: Option<PrimarySelectionDevice>,
    pub(super) primary_selection_source: Option<(PrimarySelectionSource, String)>,
    pub(super) shm: Shm,
    pub(super) mem_pool: RefCell<SlotPool>,
    pub(super) kde_blur_manager: Option<OrgKdeKwinBlurManager>,
    pub(super) seat_bindings: SeatBindings<ObjectId>,
}

impl WaylandState {
    pub(super) fn new(globals: &GlobalList, qh: &QueueHandle<Self>) -> anyhow::Result<Self> {
        let shm = Shm::bind(&globals, qh)?;
        let mem_pool = SlotPool::new(1, &shm)?;

        let compositor = CompositorState::bind(globals, qh)?;
        let subcompositor =
            SubcompositorState::bind(compositor.wl_compositor().clone(), globals, qh)?;

        let blur_manager: Option<OrgKdeKwinBlurManager> = globals.bind(qh, 1..=1, GlobalData).ok();
        let wayland_state = WaylandState {
            registry: RegistryState::new(globals),
            output: OutputState::new(globals, qh),
            compositor,
            subcompositor: Arc::new(subcompositor),
            text_input: TextInputState::bind(globals, qh).ok(),
            output_manager: if config::configuration().enable_zwlr_output_manager {
                Some(OutputManagerState::bind(globals, qh)?)
            } else {
                None
            },
            windows: RefCell::new(HashMap::new()),
            seat: SeatState::new(globals, qh),
            xdg: XdgShell::bind(globals, qh)?,
            active_surface_id: RefCell::new(None),
            last_serial: RefCell::new(0),
            keyboard: None,
            keyboard_mapper: None,
            key_repeat_rate: 25,
            key_repeat_delay: 400,
            keyboard_window_id: None,
            pointer: None,
            surface_to_pending: HashMap::new(),
            data_device_manager_state: DataDeviceManagerState::bind(globals, qh)?,
            data_device: None,
            copy_paste_source: None,
            primary_selection_manager: PrimarySelectionManagerState::bind(globals, qh).ok(),
            primary_selection_device: None,
            primary_selection_source: None,
            shm,
            mem_pool: RefCell::new(mem_pool),
            kde_blur_manager: blur_manager,
            seat_bindings: SeatBindings::default(),
        };
        Ok(wayland_state)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SeatBindings<T: Clone + Eq> {
    keyboard: Option<T>,
    pointer: Option<T>,
    data_device: Option<T>,
    primary_selection: Option<T>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RemovedSeatCleanup {
    pub(super) keyboard: bool,
    pub(super) pointer: bool,
    pub(super) data_device: bool,
    pub(super) primary_selection: bool,
}

impl<T: Clone + Eq> SeatBindings<T> {
    pub(super) fn note_keyboard(&mut self, seat: T) {
        self.keyboard = Some(seat);
    }

    pub(super) fn note_pointer(&mut self, seat: T) {
        self.pointer = Some(seat);
    }

    pub(super) fn note_data_device(&mut self, seat: T) {
        self.data_device = Some(seat);
    }

    pub(super) fn note_primary_selection(&mut self, seat: T) {
        self.primary_selection = Some(seat);
    }

    pub(super) fn clear_keyboard_if_matches(&mut self, seat: &T) -> bool {
        clear_slot_if_matches(&mut self.keyboard, seat)
    }

    pub(super) fn clear_pointer_if_matches(&mut self, seat: &T) -> bool {
        clear_slot_if_matches(&mut self.pointer, seat)
    }

    pub(super) fn clear_removed_seat(&mut self, seat: &T) -> RemovedSeatCleanup {
        RemovedSeatCleanup {
            keyboard: clear_slot_if_matches(&mut self.keyboard, seat),
            pointer: clear_slot_if_matches(&mut self.pointer, seat),
            data_device: clear_slot_if_matches(&mut self.data_device, seat),
            primary_selection: clear_slot_if_matches(&mut self.primary_selection, seat),
        }
    }
}

fn clear_slot_if_matches<T: Eq>(slot: &mut Option<T>, seat: &T) -> bool {
    if slot.as_ref() == Some(seat) {
        *slot = None;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{RemovedSeatCleanup, SeatBindings};

    #[test]
    fn removed_seat_cleanup_only_clears_matching_bindings() {
        let mut bindings = SeatBindings::default();
        bindings.note_keyboard(1_u32);
        bindings.note_pointer(1_u32);
        bindings.note_data_device(2_u32);
        bindings.note_primary_selection(1_u32);

        let cleanup = bindings.clear_removed_seat(&1_u32);

        assert_eq!(
            cleanup,
            RemovedSeatCleanup {
                keyboard: true,
                pointer: true,
                data_device: false,
                primary_selection: true,
            }
        );

        assert!(!bindings.clear_keyboard_if_matches(&1_u32));
        assert!(!bindings.clear_pointer_if_matches(&1_u32));
        assert_eq!(bindings.clear_removed_seat(&2_u32).data_device, true);
    }

    #[test]
    fn clearing_a_non_matching_capability_is_a_noop() {
        let mut bindings = SeatBindings::default();
        bindings.note_keyboard(7_u32);
        bindings.note_pointer(9_u32);

        assert!(!bindings.clear_keyboard_if_matches(&8_u32));
        assert!(!bindings.clear_pointer_if_matches(&8_u32));

        let cleanup = bindings.clear_removed_seat(&10_u32);
        assert_eq!(cleanup, RemovedSeatCleanup::default());
    }

    #[test]
    fn rebinding_keyboard_and_pointer_keeps_the_new_seat_active() {
        let mut bindings = SeatBindings::default();
        bindings.note_keyboard(1_u32);
        bindings.note_pointer(1_u32);

        bindings.note_keyboard(2_u32);
        bindings.note_pointer(2_u32);

        assert!(!bindings.clear_keyboard_if_matches(&1_u32));
        assert!(!bindings.clear_pointer_if_matches(&1_u32));

        let cleanup = bindings.clear_removed_seat(&2_u32);
        assert_eq!(
            cleanup,
            RemovedSeatCleanup {
                keyboard: true,
                pointer: true,
                data_device: false,
                primary_selection: false,
            }
        );
    }

    #[test]
    fn rebinding_selection_devices_clears_only_the_latest_owner() {
        let mut bindings = SeatBindings::default();
        bindings.note_data_device(3_u32);
        bindings.note_primary_selection(3_u32);

        bindings.note_data_device(4_u32);
        bindings.note_primary_selection(4_u32);

        assert_eq!(
            bindings.clear_removed_seat(&3_u32),
            RemovedSeatCleanup::default()
        );

        let cleanup = bindings.clear_removed_seat(&4_u32);
        assert_eq!(
            cleanup,
            RemovedSeatCleanup {
                keyboard: false,
                pointer: false,
                data_device: true,
                primary_selection: true,
            }
        );
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState, SeatState];
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {
        log::trace!("new output: OutputHandler");
    }

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {
        log::trace!("update output: OutputHandler");
    }

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {
        log::trace!("output destroyed: OutputHandler");
    }
}

delegate_registry!(WaylandState);

delegate_shm!(WaylandState);

delegate_output!(WaylandState);
delegate_compositor!(WaylandState, surface: [SurfaceData, SurfaceUserData]);
delegate_subcompositor!(WaylandState);

delegate_seat!(WaylandState);

delegate_data_device!(WaylandState);

delegate_pointer!(WaylandState, pointer: [PointerUserData]);

delegate_xdg_shell!(WaylandState);
delegate_xdg_window!(WaylandState);

delegate_primary_selection!(WaylandState);

delegate_dispatch!(WaylandState: [ZwpTextInputManagerV3: GlobalData] => TextInputState);
delegate_dispatch!(WaylandState: [ZwpTextInputV3: TextInputData] => TextInputState);

delegate_dispatch!(WaylandState: [ZwlrOutputManagerV1: GlobalData] => OutputManagerState);
delegate_dispatch!(WaylandState: [ZwlrOutputHeadV1: OutputManagerData] => OutputManagerState);
delegate_dispatch!(WaylandState: [ZwlrOutputModeV1: OutputManagerData] => OutputManagerState);
