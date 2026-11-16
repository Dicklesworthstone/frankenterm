#![cfg(target_os = "macos")]
use crate::{ToastNotification, ToastNotificationAction};
use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_foundation::{ns_string, NSArray, NSDictionary, NSError, NSSet, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
    UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, Once};

const NEEDS_SIGN: &str = "Note that the application must be code-signed \
                          for UNUserNotificationCenter to work";

static ACTION_HANDLERS: LazyLock<Mutex<HashMap<String, ToastNotificationAction>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn register_action_handler(identifier: &str, action: &ToastNotificationAction) {
    match ACTION_HANDLERS.lock() {
        Ok(mut handlers) => {
            handlers.insert(identifier.to_string(), action.clone());
        }
        Err(err) => {
            log::error!("cannot register toast action handler: {err:#}");
        }
    }
}

fn take_action_handler(identifier: &str) -> Option<ToastNotificationAction> {
    match ACTION_HANDLERS.lock() {
        Ok(mut handlers) => handlers.remove(identifier),
        Err(err) => {
            log::error!("cannot resolve toast action handler: {err:#}");
            None
        }
    }
}

fn ns_error_to_string(err: *mut NSError) -> String {
    if err.is_null() {
        "null error".to_string()
    } else {
        unsafe {
            let err: &NSError = &*err;
            format!(
                "{} {:?}",
                err.localizedDescription(),
                err.localizedFailureReason()
            )
        }
    }
}

define_class!(
    #[unsafe(super = NSObject)]
    #[name = "WezTermNotifDelegate"]
    #[derive(Debug)]
    struct NotifDelegate;

    unsafe impl NSObjectProtocol for NotifDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotifDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        unsafe fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::Block<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            log::debug!("will_present");
            let options = UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound
                | UNNotificationPresentationOptions::Badge
                | UNNotificationPresentationOptions::Banner;
            completion_handler.call((options,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        unsafe fn did_receive_notification(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &Block<dyn Fn()>,
        ) {
            let action = response.actionIdentifier();
            let request = response.notification().request();
            let identifier = request.identifier().to_string();
            let user_info = request.content().userInfo();
            let url = user_info.valueForKey(ns_string!("url"));

            log::debug!(
                "did_receive_notification -> action={action:?} identifier={identifier} url={url:?}"
            );

            let is_activation = action.isEqualToString(ns_string!("SHOW_URL"))
                || action.to_string() == "com.apple.UNNotificationDefaultActionIdentifier";

            if !is_activation {
                log::debug!(
                    "ignoring non-activation notification response action={action:?} identifier={identifier}"
                );
                take_action_handler(&identifier);
            } else if let Some(action) = take_action_handler(&identifier) {
                action.invoke();
            } else if let Some(url) = url {
                if let Ok(url_str) = url.downcast::<NSString>() {
                    frankenterm_open_url::open_url(&url_str.to_string());
                }
            }

            completion_handler.call(());
        }
    }
);

impl NotifDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        let me: Retained<Self> = unsafe { msg_send![super(this), init] };
        log::debug!("new delegate {:?}", Retained::as_ptr(&me));
        me
    }
}

impl Drop for NotifDelegate {
    fn drop(&mut self) {
        log::debug!("dropping NotifDelegate {:?}", self as *mut Self);
    }
}

/// Returns true if the process is running inside a valid macOS app bundle.
/// UNUserNotificationCenter crashes with NSInternalInconsistencyException
/// when invoked outside a bundle (e.g. `cargo run` from a build directory).
fn has_valid_bundle() -> bool {
    use objc2_foundation::NSBundle;
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some()
}

/// Once-checked flag: have we already determined bundle validity?
static BUNDLE_CHECK: LazyLock<bool> = LazyLock::new(|| {
    let valid = has_valid_bundle();
    if !valid {
        log::warn!(
            "Not running inside an app bundle; toast notifications disabled. \
             Launch from FrankenTerm.app for full notification support."
        );
    }
    valid
});

fn get_center() -> Retained<UNUserNotificationCenter> {
    UNUserNotificationCenter::currentNotificationCenter()
}

pub fn initialize() {
    if !*BUNDLE_CHECK {
        return;
    }
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let center = get_center();
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert
                | UNAuthorizationOptions::Provisional
                | UNAuthorizationOptions::Sound,
            &RcBlock::new(|ok: Bool, err| {
                if ok.is_false() {
                    log::error!(
                        "requestAuthorization status={ok:?} {}. {NEEDS_SIGN}",
                        ns_error_to_string(err)
                    );
                }
            }),
        );

        let show_url = UNNotificationAction::actionWithIdentifier_title_options(
            ns_string!("SHOW_URL"),
            ns_string!("Show"),
            UNNotificationActionOptions::empty(),
        );
        let show_url_cat =
            UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                ns_string!("SHOW_URL_ACTION"),
                &NSArray::from_retained_slice(&[show_url]),
                &NSArray::from_slice(&[]),
                UNNotificationCategoryOptions::CustomDismissAction,
            );
        center.setNotificationCategories(&NSSet::from_retained_slice(&[show_url_cat]));

        let delegate = NotifDelegate::new();
        let delegate_proto = ProtocolObject::from_retained(delegate.clone());
        center.setDelegate(Some(&delegate_proto));
        log::debug!(
            "after setDelegate {:?}, center.delegate={:?}",
            delegate,
            center.delegate()
        );

        // Intentionally "leak" the delegate.
        // I've tried stashing it into a global to keep it alive,
        // but something still manages to drop the underlying delegate
        // and that will break the weak ref in the center.
        // This is likely not the right way to do this, but after
        // spending two hours scratching my head, this is the least
        // crazy thing.
        Retained::into_raw(delegate);
    });
}

pub fn show_notif(toast: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
    if !*BUNDLE_CHECK {
        log::debug!("Skipping notification (no bundle): {}", toast.title);
        return Ok(());
    }
    initialize();
    let center = get_center();
    unsafe {
        log::debug!("show_notif center.delegate is {:?}", center.delegate());

        let notif = UNMutableNotificationContent::new();
        notif.setTitle(&NSString::from_str(&toast.title));
        notif.setBody(&NSString::from_str(&toast.message));

        let identifier = uuid::Uuid::new_v4().to_string();

        if let Some(url) = &toast.url {
            let info =
                NSDictionary::from_slices(&[ns_string!("url")], &[&*NSString::from_str(url)]);
            notif.setUserInfo(
                info.downcast_ref::<NSDictionary>()
                    .expect("is NSDictionary"),
            );
        }
        if toast.has_activation_action() {
            notif.setCategoryIdentifier(ns_string!("SHOW_URL_ACTION"));
        }

        if let Some(action) = toast.action.as_ref() {
            register_action_handler(&identifier, action);
        }
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&identifier),
            &notif,
            None,
        );

        center.addNotificationRequest_withCompletionHandler(
            &request,
            Some(&RcBlock::new(move |err: *mut NSError| {
                if err.is_null() {
                    if let Some(timeout) = toast.timeout {
                        let identifier = identifier.clone();
                        if let Err(err) = std::thread::Builder::new()
                            .name("macos-toast-timeout".to_string())
                            .spawn(move || {
                                std::thread::sleep(timeout);
                                take_action_handler(&identifier);
                                // Remove this notification
                                let ident_array =
                                    NSArray::from_retained_slice(&[NSString::from_str(
                                        &identifier,
                                    )]);
                                let c = get_center();
                                c.removeDeliveredNotificationsWithIdentifiers(&ident_array);
                            })
                        {
                            log::error!("failed to spawn macOS notification timeout: {err:#}");
                        }
                    }
                } else {
                    take_action_handler(&identifier);
                    log::error!("notif failed {}. {NEEDS_SIGN}", ns_error_to_string(err));
                }
            })),
        );
    }

    Ok(())
}
