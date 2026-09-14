#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::mut_from_ref)]
use crate::macos::{nsstring, nsstring_to_str};
use crate::superclass;
pub use cocoa::appkit::NSEventModifierFlags;
use cocoa::appkit::{NSApp, NSApplication, NSEvent, NSMenu, NSMenuItem};
pub use cocoa::base::SEL;
use cocoa::base::{id, nil};
use cocoa::foundation::NSInteger;
use config::keyassignment::KeyAssignment;
use objc::declare::ClassDecl;
use objc::rc::StrongPtr;
use objc::runtime::{Class, Object, Sel, BOOL, NO, YES};
pub use objc::*;
use std::ffi::c_void;

pub struct Menu {
    menu: StrongPtr,
}

impl Menu {
    /// Avoid populating later menus (notably macOS's dynamic Windows menu)
    /// for a likely font shortcut. Earlier submenus still get the original
    /// event in menu order, preserving collisions and user App Shortcuts.
    /// AppKit remains responsible for matching, validation and action dispatch.
    ///
    /// # Safety
    /// `event` must be a live NSEvent supplied by AppKit for this synchronous
    /// callback. This rechecks the main thread and key-down type before getters.
    /// `owns_event` must recheck the terminal first responder before each call;
    /// it must not retain a Rust borrow across synchronous menu dispatch.
    pub(crate) unsafe fn perform_font_key_equivalent(
        &self,
        event: id,
        mut owns_event: impl FnMut() -> bool,
    ) -> bool {
        let main_thread: BOOL = msg_send![class!(NSThread), isMainThread];
        if main_thread != YES || event.is_null() || !owns_event() {
            return false;
        }
        let event_type: u64 = msg_send![event, type];
        if event_type != cocoa::appkit::NSEventType::NSKeyDown as u64 {
            return false;
        }
        let delegate: id = msg_send![*self.menu, delegate];
        if !delegate.is_null() {
            // A main-menu delegate may define precedence independently of items.
            return false;
        }
        let Some(view_item) = self.item_with_title("View") else {
            return false;
        };
        let Some(view_menu) = view_item.get_sub_menu() else {
            return false;
        };
        let Some(font_item) = view_menu.item_with_title("Font Size") else {
            return false;
        };
        let Some(font_menu) = font_item.get_sub_menu() else {
            return false;
        };

        let characters = event.charactersIgnoringModifiers();
        let unshifted: id = msg_send![event, charactersByApplyingModifiers: 0_u64];
        if characters.is_null() || unshifted.is_null() {
            return false;
        }
        let characters = nsstring_to_str(characters);
        let unshifted = nsstring_to_str(unshifted);
        let modifiers = menu_key_modifiers(event.modifierFlags());
        // Only actual owned font assignments qualify. Missing, disabled-default
        // or remapped shortcuts do not acquire a hard-coded Command +/- alias.
        let candidate = font_menu.items().iter().any(|item| {
            if item.get_action() != Some(sel!(frankentermPerformKeyAssignment:)) {
                return false;
            }
            if !matches!(
                item.get_represented_item(),
                Some(RepresentedItem::KeyAssignment(
                    KeyAssignment::IncreaseFontSize
                        | KeyAssignment::DecreaseFontSize
                        | KeyAssignment::ResetFontSize
                ))
            ) {
                return false;
            }
            let key: id = msg_send![*item.item, keyEquivalent];
            let mask: NSEventModifierFlags = msg_send![*item.item, keyEquivalentModifierMask];
            !key.is_null()
                && crate::font_menu_key_candidate(
                    characters,
                    unshifted,
                    modifiers,
                    nsstring_to_str(key),
                    menu_key_modifiers(mask),
                )
        });
        if !candidate {
            return false;
        }

        let items = self.items();
        let Some(last) = items.iter().position(|item| *item.item == *view_item.item) else {
            return false;
        };
        // Keep the full View submenu intact as well: an earlier View action
        // may have the same effective shortcut. Never skip straight to Font Size.
        let mut prefix = Vec::with_capacity(last + 1);
        for item in &items[..=last] {
            let hidden: BOOL = msg_send![*item.item, isHidden];
            let enabled: BOOL = msg_send![*item.item, isEnabled];
            let key: id = msg_send![*item.item, keyEquivalent];
            // Unexpected top-level shortcuts or visibility rules need the normal
            // main-menu path. Do not invent matching semantics for these cases.
            if hidden == YES
                || enabled != YES
                || (!key.is_null() && !nsstring_to_str(key).is_empty())
            {
                return false;
            }
            let Some(menu) = item.get_sub_menu() else {
                return false;
            };
            prefix.push(menu);
        }

        crate::dispatch_key_equivalent_prefix(&prefix, last, |index, menu| {
            let item_count: NSInteger = msg_send![*self.menu, numberOfItems];
            let delegate: id = msg_send![*self.menu, delegate];
            if !owns_event()
                // itemAtIndex: raises an ObjC exception for an out-of-range
                // index. A preceding menu's update may have rebuilt the bar.
                || item_count <= last as NSInteger
                || !delegate.is_null()
                || Self::get_main_menu().is_none_or(|current| *current.menu != *self.menu)
                || self.item_at_index(index).is_none_or(|current| {
                    let hidden: BOOL = msg_send![*current.item, isHidden];
                    let enabled: BOOL = msg_send![*current.item, isEnabled];
                    let key: id = msg_send![*current.item, keyEquivalent];
                    hidden == YES
                        || enabled != YES
                        || (!key.is_null() && !nsstring_to_str(key).is_empty())
                        || *current.item != *items[index].item
                        || current
                            .get_sub_menu()
                            .is_none_or(|current| *current.menu != *menu.menu)
                })
                || view_item
                    .get_sub_menu()
                    .is_none_or(|current| *current.menu != *view_menu.menu)
                || font_item
                    .get_sub_menu()
                    .is_none_or(|current| *current.menu != *font_menu.menu)
                || self
                    .item_at_index(last)
                    .is_none_or(|current| *current.item != *view_item.item)
            {
                return None;
            }
            // May synchronously invoke WindowView::frankenterm_perform_key_assignment.
            // No WindowInner borrow is held here.
            let handled: BOOL = msg_send![*menu.menu, performKeyEquivalent: event];
            if handled == YES {
                log::debug!(
                    target: "window::font_menu_profile",
                    "event=font_menu_accelerator_dispatch route={}",
                    if index == last { "font_parent" } else { "preceding_menu" },
                );
            }
            Some(handled == YES)
        })
    }

    pub fn new_with_title(title: &str) -> Self {
        unsafe {
            let menu = NSMenu::alloc(nil);
            let menu = StrongPtr::new(menu.initWithTitle_(*nsstring(title)));
            Self { menu }
        }
    }

    pub fn autorelease(self) -> *mut Object {
        self.menu.autorelease()
    }

    pub fn item_at_index(&self, index: usize) -> Option<MenuItem> {
        let index = index as i64;
        let item = unsafe { self.menu.itemAtIndex_(index) };
        if item.is_null() {
            None
        } else {
            Some(MenuItem {
                item: unsafe { StrongPtr::retain(item) },
            })
        }
    }

    pub fn assign_as_main_menu(&self) {
        unsafe {
            let ns_app = NSApp();
            ns_app.setMainMenu_(*self.menu);
        }
    }

    pub fn get_main_menu() -> Option<Self> {
        unsafe {
            let ns_app = NSApp();
            let existing = ns_app.mainMenu();
            if existing.is_null() {
                None
            } else {
                Some(Self {
                    menu: StrongPtr::retain(existing),
                })
            }
        }
    }

    pub fn assign_as_help_menu(&self) {
        unsafe {
            let ns_app = NSApp();
            let () = msg_send![ns_app, setHelpMenu:*self.menu];
        }
    }

    pub fn assign_as_windows_menu(&self) {
        unsafe {
            let ns_app = NSApp();
            ns_app.setWindowsMenu_(*self.menu);
        }
    }

    pub fn assign_as_services_menu(&self) {
        unsafe {
            let ns_app = NSApp();
            ns_app.setServicesMenu_(*self.menu);
        }
    }

    pub fn assign_as_app_menu(&self) {
        unsafe {
            let ns_app = NSApp();
            let () = msg_send![ns_app, performSelector:sel!(setAppleMenu:) withObject:*self.menu];
        }
    }

    pub fn add_item(&self, item: &MenuItem) {
        unsafe {
            self.menu.addItem_(*item.item);
        }
    }

    pub fn item_with_title(&self, title: &str) -> Option<MenuItem> {
        unsafe {
            let item: id = msg_send![*self.menu, itemWithTitle:*nsstring(title)];
            if item.is_null() {
                None
            } else {
                Some(MenuItem {
                    item: StrongPtr::retain(item),
                })
            }
        }
    }

    pub fn get_or_create_sub_menu<F: FnOnce(&Menu)>(&self, title: &str, on_create: F) -> Menu {
        match self.item_with_title(title) {
            Some(m) => m.get_sub_menu().unwrap(),
            None => {
                let item = MenuItem::new_with(title, None, "");
                let menu = Menu::new_with_title(title);
                item.set_sub_menu(&menu);
                self.add_item(&item);
                on_create(&menu);
                menu
            }
        }
    }

    pub fn get_sub_menu(&self, title: &str) -> Menu {
        self.item_with_title(title).unwrap().get_sub_menu().unwrap()
    }

    pub fn remove_all_items(&self) {
        unsafe {
            let () = msg_send![*self.menu, removeAllItems];
        }
    }

    pub fn remove_item(&self, item: &MenuItem) {
        unsafe {
            let () = msg_send![*self.menu, removeItem:*item.item];
        }
    }

    pub fn items(&self) -> Vec<MenuItem> {
        unsafe {
            let n: NSInteger = msg_send![*self.menu, numberOfItems];
            let mut items = vec![];
            for i in 0..n {
                items.push(self.item_at_index(i as _).expect("index to be valid"));
            }
            items
        }
    }

    pub fn index_of_item_with_represented_object(&self, object: id) -> Option<usize> {
        unsafe {
            let n: NSInteger = msg_send![*self.menu, indexOfItemWithRepresentedObject: object];
            if n == -1 {
                None
            } else {
                Some(n as usize)
            }
        }
    }

    pub fn index_of_item_with_represented_item(&self, item: &RepresentedItem) -> Option<usize> {
        let wrapped = item.clone().wrap();
        self.index_of_item_with_represented_object(*wrapped)
    }

    pub fn get_item_with_represented_item(&self, item: &RepresentedItem) -> Option<MenuItem> {
        let idx = self.index_of_item_with_represented_item(item)?;
        self.item_at_index(idx)
    }
}

fn menu_key_modifiers(flags: NSEventModifierFlags) -> crate::Modifiers {
    let mut modifiers = crate::Modifiers::NONE;
    for (native, portable) in [
        (
            NSEventModifierFlags::NSCommandKeyMask,
            crate::Modifiers::SUPER,
        ),
        (
            NSEventModifierFlags::NSControlKeyMask,
            crate::Modifiers::CTRL,
        ),
        (
            NSEventModifierFlags::NSAlternateKeyMask,
            crate::Modifiers::ALT,
        ),
        (
            NSEventModifierFlags::NSShiftKeyMask,
            crate::Modifiers::SHIFT,
        ),
    ] {
        modifiers.set(portable, flags.contains(native));
    }
    modifiers
}

pub struct MenuItem {
    item: StrongPtr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RepresentedItem {
    KeyAssignment(KeyAssignment),
}

impl RepresentedItem {
    fn wrap(self) -> StrongPtr {
        let wrapper: id = unsafe { msg_send![get_wrapper_class(), alloc] };
        let wrapper = unsafe { StrongPtr::new(wrapper) };
        let item = Box::new(self);
        let item: *const RepresentedItem = Box::into_raw(item);
        let item = item as *const c_void;
        unsafe {
            (**wrapper).set_ivar(WRAPPER_FIELD_NAME, item);
        }
        wrapper
    }

    unsafe fn ref_item(wrapper: id) -> Option<RepresentedItem> {
        let item = (*wrapper).get_ivar::<*const c_void>(WRAPPER_FIELD_NAME);
        let item = (*item) as *const RepresentedItem;
        if item.is_null() {
            None
        } else {
            Some((*item).clone())
        }
    }
}

impl MenuItem {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn with_menu_item(item: id) -> Self {
        let item = unsafe { StrongPtr::retain(item) };
        Self { item }
    }

    pub fn new_separator() -> Self {
        let item = unsafe { StrongPtr::new(NSMenuItem::separatorItem(nil)) };
        Self { item }
    }

    pub fn new_with(title: &str, action: Option<SEL>, key: &str) -> Self {
        unsafe {
            let item = NSMenuItem::alloc(nil);
            let item = item.initWithTitle_action_keyEquivalent_(
                *nsstring(title),
                action.unwrap_or_else(|| SEL::from_ptr(std::ptr::null())),
                *nsstring(key),
            );

            Self {
                item: StrongPtr::new(item),
            }
        }
    }

    pub fn get_action(&self) -> Option<SEL> {
        unsafe {
            let s: SEL = msg_send![*self.item, action];
            if s.as_ptr().is_null() {
                None
            } else {
                Some(s)
            }
        }
    }

    pub fn set_tool_tip(&self, tip: &str) {
        unsafe {
            let () = msg_send![*self.item, setToolTip:*nsstring(tip)];
        }
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn set_target(&self, target: id) {
        unsafe {
            self.item.setTarget_(target);
        }
    }

    pub fn set_sub_menu(&self, menu: &Menu) {
        unsafe {
            self.item.setSubmenu_(*menu.menu);
        }
    }

    pub fn get_sub_menu(&self) -> Option<Menu> {
        unsafe {
            let menu: id = msg_send![*self.item, submenu];
            if menu.is_null() {
                None
            } else {
                Some(Menu {
                    menu: StrongPtr::retain(menu),
                })
            }
        }
    }

    pub fn get_parent_item(&self) -> Option<Self> {
        unsafe {
            let item: id = msg_send![*self.item, parentItem];
            if item.is_null() {
                None
            } else {
                Some(Self {
                    item: StrongPtr::retain(item),
                })
            }
        }
    }

    pub fn get_menu(&self) -> Option<Menu> {
        unsafe {
            let item: id = msg_send![*self.item, menu];
            if item.is_null() {
                None
            } else {
                Some(Menu {
                    menu: StrongPtr::retain(item),
                })
            }
        }
    }

    /// Set an integer tag to identify this item
    pub fn set_tag(&self, tag: NSInteger) {
        unsafe {
            let () = msg_send![*self.item, setTag: tag];
        }
    }

    pub fn get_title(&self) -> String {
        unsafe {
            let title: id = msg_send![*self.item, title];
            nsstring_to_str(title).to_string()
        }
    }

    pub fn set_title(&self, title: &str) {
        unsafe {
            let () = msg_send![*self.item, setTitle:*nsstring(title)];
        }
    }

    pub fn set_key_equivalent(&self, equiv: &str) {
        unsafe {
            let () = msg_send![*self.item, setKeyEquivalent:*nsstring(equiv)];
        }
    }

    pub fn get_tag(&self) -> NSInteger {
        unsafe { msg_send![*self.item, tag] }
    }

    /// Associate the item to an object
    fn set_represented_object(&self, object: id) {
        unsafe {
            let () = msg_send![*self.item, setRepresentedObject: object];
        }
    }

    fn get_represented_object(&self) -> Option<StrongPtr> {
        unsafe {
            let object: id = msg_send![*self.item, representedObject];
            if object.is_null() {
                None
            } else {
                Some(StrongPtr::retain(object))
            }
        }
    }

    pub fn set_represented_item(&self, item: RepresentedItem) {
        let wrapper = item.wrap();
        self.set_represented_object(*wrapper);
    }

    pub fn get_represented_item(&self) -> Option<RepresentedItem> {
        let wrapper = self.get_represented_object()?;
        unsafe { RepresentedItem::ref_item(*wrapper) }
    }

    pub fn set_key_equiv_modifier_mask(&self, mods: NSEventModifierFlags) {
        unsafe {
            let () = msg_send![*self.item, setKeyEquivalentModifierMask: mods];
        }
    }
}

const WRAPPER_CLS_NAME: &str = "FrankenTermNSMenuRepresentedItem";
const WRAPPER_FIELD_NAME: &str = "item";
/// Wraps RepresentedItem in an NSObject so that we can associate
/// it with a MenuItem
fn get_wrapper_class() -> &'static Class {
    Class::get(WRAPPER_CLS_NAME).unwrap_or_else(|| {
        let mut cls =
            ClassDecl::new(WRAPPER_CLS_NAME, class!(NSObject)).expect("Unable to register class");

        extern "C" fn dealloc(this: &mut Object, _sel: Sel) {
            unsafe {
                let item = this.get_ivar::<*mut c_void>(WRAPPER_FIELD_NAME);
                let item = (*item) as *mut RepresentedItem;
                let item = Box::from_raw(item);
                drop(item);
                let superclass = superclass(this);
                let () = msg_send![super(this, superclass), dealloc];
            }
        }

        extern "C" fn is_equal(this: &mut Object, _sel: Sel, that: *mut Object) -> BOOL {
            unsafe {
                let this_item = RepresentedItem::ref_item(this);
                let that_item = RepresentedItem::ref_item(that);
                if this_item == that_item {
                    YES
                } else {
                    NO
                }
            }
        }

        cls.add_ivar::<*mut c_void>(WRAPPER_FIELD_NAME);
        unsafe {
            cls.add_method(sel!(dealloc), dealloc as extern "C" fn(&mut Object, Sel));
            cls.add_method(
                sel!(isEqual:),
                is_equal as extern "C" fn(&mut Object, Sel, *mut Object) -> BOOL,
            );
        }
        cls.register()
    })
}
