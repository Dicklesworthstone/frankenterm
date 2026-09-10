#![cfg(windows)]

use crate::ToastNotification as TN;
use xml::escape::{escape_str_attribute, escape_str_pcdata};

use windows::core::{IInspectable, Interface, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
};

fn show_notif_impl(toast: TN) -> Result<(), Box<dyn std::error::Error>> {
    let xml = XmlDocument::new()?;

    let actions = if toast.has_activation_action() {
        format!(
            r#"
        <actions>
           <action content="{}" arguments="show" />
        </actions>
"#,
            escape_str_attribute(toast.activation_label())
        )
    } else {
        String::new()
    };

    xml.LoadXml(&HSTRING::from(format!(
        r#"<toast duration="long">
        <visual>
            <binding template="ToastGeneric">
                <text>{}</text>
                <text>{}</text>
            </binding>
        </visual>
        {}
    </toast>"#,
        escape_str_pcdata(&toast.title),
        escape_str_pcdata(&toast.message),
        actions
    )))?;

    let notif = ToastNotification::CreateToastNotification(&xml)?;

    notif.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
        move |_, result| {
            let result = result.ok()?.cast::<ToastActivatedEventArgs>()?;

            let args = result.Arguments()?;

            if args == "show" && toast.has_activation_action() {
                toast.activate();
            }

            Ok(())
        },
    ))?;

    /*
    notif.dismissed(TypedEventHandler::new(|sender, result| {
        log::info!("dismissed {:?}", result);
        Ok(())
    }))?;

    notif.failed(TypedEventHandler::new(|sender, result| {
        log::warn!("toasts are disabled {:?}", result);
        Ok(())
    }))?;
    */

    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("com.frankenterm.gui"))?;

    notifier.Show(&notif)?;

    Ok(())
}

pub fn show_notif(notif: TN) -> Result<(), Box<dyn std::error::Error>> {
    // We need to be in a different thread from the caller
    // in case we get called in the guts of a windows message
    // loop dispatch and are unable to pump messages
    std::thread::Builder::new()
        .name("windows-toast-notification".to_string())
        .spawn(move || {
            if let Err(err) = show_notif_impl(notif) {
                log::error!("Failed to show toast notification: {:#}", err);
            }
        })?;

    Ok(())
}
