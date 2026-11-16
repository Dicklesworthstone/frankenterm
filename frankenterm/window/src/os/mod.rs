#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(all(feature = "wayland", unix, not(target_os = "macos")))]
pub mod wayland;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod x_and_wayland;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod xdg_desktop_portal;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod xkeysyms;

#[cfg(all(unix, not(target_os = "macos")))]
pub use x_and_wayland::*;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::*;

pub mod parameters;
