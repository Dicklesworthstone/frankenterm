//! A window's display link (ft-yccm0.4.1.3): a `CAMetalDisplayLink` that
//! paces the window's render thread.
//!
//! The link is scheduled on the render thread's own run loop, in a private
//! mode that nothing else uses, so its callbacks run on that thread and
//! nowhere else. The render thread waits by running its run loop in that mode
//! ([`MetalDisplayLink::wait`]): the link's callback stores the update (the
//! drawable for the next refresh and its target presentation time) and stops
//! the run loop, so the wait returns with it. Other threads interrupt a wait
//! through a [`RunLoopInterrupter`]. It enqueues a block that stops the run
//! loop the next time it runs in that mode, so an interrupt that comes just
//! before the wait begins is not lost.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopRunResult, CFString, CFType,
};
use objc2_foundation::{NSObjectProtocol, NSRunLoop, NSString};
use objc2_quartz_core::{
    CAFrameRateRange, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate,
    CAMetalDrawable, CAMetalLayer,
};
use std::cell::RefCell;
use std::time::Duration;

/// The run loop mode the link is scheduled in: the render thread's own.
const RUN_LOOP_MODE: &str = "FrankenTermRenderThread";

/// The longest single wait, in seconds, when the caller set no timeout.
const FOREVER: f64 = 86_400.0;

/// One display refresh from the link: the drawable to render the next frame
/// into, ready now, and when it will be shown.
pub struct LinkUpdate {
    pub(crate) drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>,
    target_presentation: f64,
}

impl LinkUpdate {
    /// When the drawable will be shown, in host time seconds
    /// (`CACurrentMediaTime`).
    #[must_use]
    pub fn target_presentation_timestamp(&self) -> f64 {
        self.target_presentation
    }
}

impl std::fmt::Debug for LinkUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkUpdate")
            .field("target_presentation", &self.target_presentation)
            .finish_non_exhaustive()
    }
}

/// Why [`MetalDisplayLink::wait`] returned.
#[derive(Debug)]
pub enum LinkWait {
    Update(LinkUpdate),
    Interrupted,
    TimedOut,
}

#[derive(Default)]
struct DelegateIvars {
    update: RefCell<Option<LinkUpdate>>,
}

// `define_class!` declares an Objective-C class, which needs `unsafe`
// attributes; see the crate docs, category 12.
#[allow(unsafe_code)]
mod delegate {
    use super::{
        CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate, CFRunLoop,
        DefinedClass, DelegateIvars, LinkUpdate, NSObject, NSObjectProtocol, define_class,
    };

    define_class!(
        // SAFETY: DISPLAY-LINK-DELEGATE. NSObject has no subclassing
        // requirements, and this class does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[ivars = DelegateIvars]
        pub(super) struct LinkDelegate;

        unsafe impl NSObjectProtocol for LinkDelegate {}

        // SAFETY: DISPLAY-LINK-DELEGATE. The method has the protocol's
        // selector and exact argument types.
        unsafe impl CAMetalDisplayLinkDelegate for LinkDelegate {
            #[unsafe(method(metalDisplayLink:needsUpdate:))]
            fn metal_display_link_needs_update(
                &self,
                _link: &CAMetalDisplayLink,
                update: &CAMetalDisplayLinkUpdate,
            ) {
                *self.ivars().update.borrow_mut() = Some(LinkUpdate {
                    drawable: update.drawable(),
                    target_presentation: update.targetPresentationTimestamp(),
                });
                // The callback runs inside the render thread's wait: end it,
                // whatever kind of run loop source delivered the callback.
                if let Some(run_loop) = CFRunLoop::current() {
                    run_loop.stop();
                }
            }
        }
    );
}

use delegate::LinkDelegate;

impl LinkDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars::default());
        #[allow(unsafe_code)]
        // SAFETY: DISPLAY-LINK-DELEGATE. NSObject's `init` takes no
        // arguments and returns the initialized object.
        unsafe {
            msg_send![super(this), init]
        }
    }
}

/// A `CAMetalDisplayLink` on the calling thread's run loop. It must be used
/// only on the thread that created it.
pub struct MetalDisplayLink {
    link: Retained<CAMetalDisplayLink>,
    delegate: Retained<LinkDelegate>,
    run_loop: CFRetained<CFRunLoop>,
    mode: CFRetained<CFString>,
}

impl std::fmt::Debug for MetalDisplayLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetalDisplayLink")
            .field("paused", &self.link.isPaused())
            .finish_non_exhaustive()
    }
}

impl MetalDisplayLink {
    /// A paused display link for `layer` on the calling thread's run loop.
    /// `None` before macOS 14, which has no `CAMetalDisplayLink`.
    pub(crate) fn new(layer: &CAMetalLayer) -> Option<Self> {
        AnyClass::get(c"CAMetalDisplayLink")?;
        let run_loop = CFRunLoop::current()?;
        let link = CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), layer);
        let delegate = LinkDelegate::new();
        // The delegate is a weak property: `self.delegate` keeps it alive.
        link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        link.setPaused(true);
        let mode = NSString::from_str(RUN_LOOP_MODE);
        #[allow(unsafe_code)]
        // SAFETY: FFI-RUNLOOP. The run loop is the calling thread's own, the
        // only thread that runs it or uses this link; the mode is a valid
        // string.
        unsafe {
            link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), &mode);
        }
        Some(Self {
            link,
            delegate,
            run_loop,
            mode: CFString::from_str(RUN_LOOP_MODE),
        })
    }

    /// Starts or pauses the link's callbacks.
    pub fn set_paused(&self, paused: bool) {
        self.link.setPaused(paused);
    }

    /// Asks the display for a refresh rate, in frames per second.
    pub fn set_frame_rate(&self, minimum: f32, maximum: f32, preferred: f32) {
        self.link.setPreferredFrameRateRange(CAFrameRateRange {
            minimum,
            maximum,
            preferred,
        });
    }

    /// Runs this thread's run loop in the link's mode until the link
    /// delivers an update, another thread interrupts, or `timeout` passes.
    pub fn wait(&self, timeout: Option<Duration>) -> LinkWait {
        if let Some(update) = self.take_update() {
            return LinkWait::Update(update);
        }
        let seconds = timeout.map_or(FOREVER, |timeout| timeout.as_secs_f64());
        let mode: &CFRunLoopMode = &self.mode;
        let result = CFRunLoop::run_in_mode(Some(mode), seconds, true);
        if let Some(update) = self.take_update() {
            return LinkWait::Update(update);
        }
        if result == CFRunLoopRunResult::TimedOut {
            LinkWait::TimedOut
        } else if result == CFRunLoopRunResult::Finished {
            // The mode has no source left to wait on: do not spin.
            std::thread::park_timeout(timeout.unwrap_or(Duration::from_millis(100)));
            LinkWait::Interrupted
        } else {
            LinkWait::Interrupted
        }
    }

    /// Interrupts a wait on this link from any thread.
    #[must_use]
    pub fn interrupter(&self) -> RunLoopInterrupter {
        RunLoopInterrupter {
            run_loop: self.run_loop.clone(),
            mode: self.mode.clone(),
        }
    }

    fn take_update(&self) -> Option<LinkUpdate> {
        self.delegate.ivars().update.borrow_mut().take()
    }
}

impl Drop for MetalDisplayLink {
    fn drop(&mut self) {
        // Removes the link from every run loop and releases its target.
        self.link.invalidate();
    }
}

/// Interrupts [`MetalDisplayLink::wait`] from any thread.
pub struct RunLoopInterrupter {
    run_loop: CFRetained<CFRunLoop>,
    mode: CFRetained<CFString>,
}

#[allow(unsafe_code)]
// SAFETY: THREAD-RUNLOOP. The run loop is only used for
// `CFRunLoopPerformBlock` and `CFRunLoopWakeUp`, which Core Foundation
// documents as callable from any thread; the mode is an immutable string.
unsafe impl Send for RunLoopInterrupter {}

#[allow(unsafe_code)]
// SAFETY: THREAD-RUNLOOP. As for `Send`: both calls are thread-safe, and
// neither needs exclusive access.
unsafe impl Sync for RunLoopInterrupter {}

impl RunLoopInterrupter {
    /// Ends the link thread's current wait, or its next one if it is not
    /// waiting now: the stop is queued until its run loop runs in the link's
    /// mode.
    pub fn interrupt(&self) {
        let block = block2::RcBlock::new(|| {
            if let Some(run_loop) = CFRunLoop::current() {
                run_loop.stop();
            }
        });
        let mode: &CFType = &self.mode;
        #[allow(unsafe_code)]
        // SAFETY: THREAD-RUNLOOP. `CFRunLoopPerformBlock` may be called from
        // any thread; the mode is a CFString naming the link's run loop
        // mode, and the block is a valid, non-null block that Core
        // Foundation copies.
        unsafe {
            self.run_loop.perform_block(Some(mode), Some(&*block));
        }
        self.run_loop.wake_up();
    }
}

impl std::fmt::Debug for RunLoopInterrupter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLoopInterrupter").finish_non_exhaustive()
    }
}
