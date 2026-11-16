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

#[cfg(test)]
mod block_abi_regression {
    use block::{ConcreteBlock, RcBlock};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct Capture {
        drops: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        owned: String,
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Capture {
        fn invoke(&self, a: u64, b: u64) -> u64 {
            self.calls.fetch_add(1, Ordering::SeqCst);
            a.wrapping_mul(3).wrapping_add(b) + self.owned.len() as u64
        }
    }

    #[test]
    fn native_block_stack_heap_copy_invoke_and_final_dispose() {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let heap: RcBlock<(u64, u64), u64> = {
            let capture = Capture {
                drops: Arc::clone(&drops),
                calls: Arc::clone(&calls),
                owned: String::from("native-capture"),
            };
            let stack = ConcreteBlock::new(move |a: u64, b: u64| capture.invoke(a, b));
            // SAFETY: exact argument/return ABI; immutable capture, atomic counts.
            // This is the existing macOS FFI boundary, not an arbitrary callback.
            assert_eq!(unsafe { stack.call((7, 11)) }, 46);
            // Invokes libSystem _Block_copy: stack ownership moves to heap.
            stack.copy()
        };
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        // Heap copy invokes the runtime retain path, not Rust closure Clone.
        let retained = heap.clone();
        drop(heap);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        // SAFETY: retained runtime block remains live after its creation scope;
        // the argument tuple and return type still match its invocation function.
        assert_eq!(unsafe { retained.call((13, 17)) }, 70);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(retained);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
