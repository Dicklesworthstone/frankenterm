#![allow(unexpected_cfgs)]
// <https://github.com/SSheldon/rust-objc/issues/125>
// The whole macOS backend is written against the `cocoa`/`objc` crates, which
// are deprecated wholesale in favour of `objc2`. Migrating is a large port
// of inherited upstream WezTerm code, not something to do piecemeal per call
// site; until then the deprecation lint would fail every
// `-D warnings` gate run on macOS while changing nothing about behaviour.
#![allow(deprecated)]
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use objc::rc::StrongPtr;
use objc::runtime::Object;
use objc::*;

mod app;
pub mod bitmap;
pub mod clipboard;
pub mod connection;
pub mod menu;
pub mod window;

mod keycodes;

pub use self::window::*;
pub use bitmap::*;
pub use connection::*;

/// Convert a rust string to a cocoa string
fn nsstring(s: &str) -> StrongPtr {
    unsafe { StrongPtr::new(NSString::alloc(nil).init_str(s)) }
}

unsafe fn nsstring_to_str<'a>(mut ns: *mut Object) -> &'a str {
    let is_astring: bool = msg_send![ns, isKindOfClass: class!(NSAttributedString)];
    if is_astring {
        ns = msg_send![ns, string];
    }
    let data = NSString::UTF8String(ns as id) as *const u8;
    let len = NSString::len(ns as id);
    let bytes = std::slice::from_raw_parts(data, len);
    std::str::from_utf8_unchecked(bytes)
}
