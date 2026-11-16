//! GuiWin represents a Gui TermWindow (as opposed to a Mux window) in lua code
use super::luaerr;
use crate::TermWindow;
use crate::termwindow::TermWindowNotif;
use config::keyassignment::{ClipboardCopyDestination, KeyAssignment};
use flume::bounded;
use frankenterm_gui::window_request::{reserve_window_request, window_request_unanswered};
use frankenterm_toast_notification::ToastNotification;
use luahelper::*;
use mlua::{UserData, UserDataMethods, UserDataRef};
use mux::Mux;
use mux::pane::PaneId;
use mux::window::WindowId as MuxWindowId;
use mux_lua::MuxPane;
use termwiz_funcs::lines_to_escapes;
use wezterm_dynamic::{FromDynamic, ToDynamic};
use window::{Connection, ConnectionOps, DeadKeyStatus, WindowOps, WindowState};

#[derive(Clone)]
pub struct GuiWin {
    pub mux_window_id: MuxWindowId,
    pub window: ::window::Window,
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
struct GuiWindowDimensions {
    pixel_width: usize,
    pixel_height: usize,
    dpi: usize,
    is_full_screen: bool,
    is_maximized: bool,
    is_hidden: bool,
    is_always_on_top: bool,
    is_always_on_bottom: bool,
    can_resize: bool,
    can_paint: bool,
    window_level: config::window::WindowLevel,
}

impl_lua_conversion_dynamic!(GuiWindowDimensions);

fn gui_window_dimensions(
    dims: ::window::Dimensions,
    window_state: WindowState,
) -> GuiWindowDimensions {
    GuiWindowDimensions {
        pixel_width: dims.pixel_width,
        pixel_height: dims.pixel_height,
        dpi: dims.dpi,
        is_full_screen: window_state.contains(WindowState::FULL_SCREEN),
        is_maximized: window_state.contains(WindowState::MAXIMIZED),
        is_hidden: window_state.contains(WindowState::HIDDEN),
        is_always_on_top: window_state.contains(WindowState::ALWAYS_ON_TOP),
        is_always_on_bottom: window_state.contains(WindowState::ALWAYS_ON_BOTTOM),
        can_resize: window_state.can_resize(),
        can_paint: window_state.can_paint(),
        window_level: window_state.as_window_level(),
    }
}

/// Send `notif` to `window` and wait for the reply on `rx`. Admission is
/// reserved first, so a busy GUI is waited for and a refusal is named; see
/// `frankenterm_gui::window_request`.
async fn ask_window<T>(
    window: &::window::Window,
    method: &'static str,
    notif: TermWindowNotif,
    rx: flume::Receiver<T>,
) -> mlua::Result<T> {
    let reservation = reserve_window_request(method).await.map_err(luaerr)?;
    window
        .notify_with_reservation(notif, reservation, None)
        .detach();
    rx.recv_async()
        .await
        .map_err(|_| luaerr(window_request_unanswered(method)))
}

impl GuiWin {
    pub fn try_new(term_window: &TermWindow) -> Option<Self> {
        let window = term_window.window.clone()?;
        Some(Self {
            window,
            mux_window_id: term_window.mux_window_id,
        })
    }
}

impl UserData for GuiWin {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, _: ()| {
            Ok(format!(
                "GuiWin(mux_window_id:{}, pid:{})",
                this.mux_window_id,
                unsafe { libc::getpid() }
            ))
        });

        methods.add_method("window_id", |_, this, _: ()| Ok(this.mux_window_id));
        methods.add_method("mux_window", |_, this, _: ()| {
            Ok(mux_lua::MuxWindow(this.mux_window_id))
        });
        methods.add_method("active_tab", |_, this, _: ()| {
            let mux = Mux::try_get().ok_or_else(|| mlua::Error::external("cannot get Mux!?"))?;
            let window = mux.get_window(this.mux_window_id).ok_or_else(|| {
                mlua::Error::external(format!("invalid window {}", this.mux_window_id))
            })?;
            Ok(window.get_active().map(|tab| mux_lua::MuxTab(tab.tab_id())))
        });

        methods.add_method(
            "set_inner_size",
            |_, this, (width, height): (usize, usize)| {
                this.window
                    .notify(TermWindowNotif::SetInnerSize { width, height });
                Ok(())
            },
        );
        methods.add_method("set_position", |_, this, (x, y): (isize, isize)| {
            this.window.set_window_position(euclid::point2(x, y));
            Ok(())
        });
        methods.add_method("maximize", |_, this, _: ()| {
            this.window.maximize();
            Ok(())
        });
        methods.add_method("restore", |_, this, _: ()| {
            this.window.restore();
            Ok(())
        });
        methods.add_method("toggle_fullscreen", |_, this, _: ()| {
            this.window.toggle_fullscreen();
            Ok(())
        });
        methods.add_method("focus", |_, this, _: ()| {
            this.window.focus();
            Ok(())
        });
        methods.add_method(
            "toast_notification",
            |_, _, (title, message, url, timeout): (String, String, Option<String>, Option<u64>)| {
                frankenterm_toast_notification::show(ToastNotification {
                    title,
                    message,
                    url,
                    action: None,
                    timeout: timeout.map(std::time::Duration::from_millis)
                });
                Ok(())
            },
        );
        methods.add_method("get_appearance", |_, _, _: ()| {
            Ok(Connection::get()
                .map(|c| c.get_appearance().to_string())
                .unwrap_or_else(|| "Dark".to_string()))
        });
        methods.add_method("set_right_status", |_, this, status: String| {
            this.window.notify(TermWindowNotif::SetRightStatus(status));
            Ok(())
        });
        methods.add_method("set_left_status", |_, this, status: String| {
            this.window.notify(TermWindowNotif::SetLeftStatus(status));
            Ok(())
        });
        methods.add_async_method("get_dimensions", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::GetDimensions(tx);
            let (dims, window_state) =
                ask_window(&this.window, "get_dimensions", notif, rx).await?;

            Ok(gui_window_dimensions(dims, window_state))
        });
        methods.add_async_method(
            "get_selection_text_for_pane",
            |_, this, pane: UserDataRef<MuxPane>| async move {
                let (tx, rx) = bounded(1);
                let notif = TermWindowNotif::GetSelectionForPane {
                    pane_id: (*pane).0,
                    tx,
                };
                let text = ask_window(&this.window, "get_selection_text_for_pane", notif, rx)
                    .await?
                    .map_err(luaerr)?;

                Ok(text)
            },
        );
        methods.add_async_method("current_event", |lua, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(term_window.current_event.to_dynamic()).ok();
            }));
            let result = ask_window(&this.window, "current_event", notif, rx).await?;
            luahelper::dynamic_to_lua_value(&lua, result)
        });
        methods.add_async_method(
            "perform_action",
            |_, this, (assignment, pane): (KeyAssignment, UserDataRef<MuxPane>)| async move {
                let (tx, rx) = bounded(1);
                let notif = TermWindowNotif::PerformAssignment {
                    pane_id: (*pane).0,
                    assignment,
                    tx: Some(tx),
                };
                let result = ask_window(&this.window, "perform_action", notif, rx).await?;
                result.map_err(mlua::Error::external)
            },
        );
        methods.add_async_method("effective_config", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::GetEffectiveConfig(tx);
            let config = ask_window(&this.window, "effective_config", notif, rx).await?;

            Ok((*config).clone())
        });
        methods.add_async_method("get_config_overrides", |lua, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::GetConfigOverrides(tx);
            let overrides = ask_window(&this.window, "get_config_overrides", notif, rx).await?;

            dynamic_to_lua_value(&lua, overrides)
        });
        methods.add_method("set_config_overrides", |_, this, value: mlua::Value| {
            let value = lua_value_to_dynamic(value)?;
            this.window
                .notify(TermWindowNotif::SetConfigOverrides(value));
            Ok(())
        });
        methods.add_async_method("is_focused", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(term_window.focused.is_some()).ok();
            }));
            ask_window(&this.window, "is_focused", notif, rx).await
        });
        methods.add_async_method("leader_is_active", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(term_window.leader_is_active()).ok();
            }));
            ask_window(&this.window, "leader_is_active", notif, rx).await
        });
        methods.add_async_method("composition_status", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(match term_window.composition_status() {
                    DeadKeyStatus::None => None,
                    DeadKeyStatus::Composing(s) => Some(s.clone()),
                })
                .ok();
            }));
            ask_window(&this.window, "composition_status", notif, rx).await
        });
        methods.add_async_method("active_key_table", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(term_window.current_key_table_name()).ok();
            }));
            ask_window(&this.window, "active_key_table", notif, rx).await
        });
        methods.add_async_method("keyboard_modifiers", |_, this, _: ()| async move {
            let (tx, rx) = bounded(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(term_window.current_modifier_and_led_state())
                    .ok();
            }));
            let (mods, leds) = ask_window(&this.window, "keyboard_modifiers", notif, rx).await?;

            Ok((mods.to_string(), leds.to_string()))
        });
        methods.add_async_method("active_pane", |_, this, _: ()| async move {
            let (tx, rx) = bounded::<Option<MuxPane>>(1);
            let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                tx.try_send(
                    term_window
                        .get_active_pane_or_overlay()
                        .map(|pane| MuxPane(pane.pane_id())),
                )
                .ok();
            }));
            ask_window(&this.window, "active_pane", notif, rx).await
        });
        methods.add_method("active_workspace", |_, _, _: ()| {
            let mux = Mux::try_get()
                .ok_or_else(|| anyhow::anyhow!("no mux?"))
                .map_err(luaerr)?;
            Ok(mux.active_workspace().to_string())
        });
        methods.add_method(
            "copy_to_clipboard",
            |_, this, (text, clipboard): (String, Option<ClipboardCopyDestination>)| {
                let clipboard = clipboard.unwrap_or_default();
                this.window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.copy_to_clipboard(clipboard, text);
                    })));
                Ok(())
            },
        );
        methods.add_async_method(
            "get_selection_escapes_for_pane",
            |_, this, pane: UserDataRef<MuxPane>| async move {
                let (tx, rx) = bounded(1);
                let pane_id = (*pane).0;
                let notif = TermWindowNotif::Apply(Box::new(move |term_window| {
                    fn do_it(
                        pane_id: PaneId,
                        term_window: &mut TermWindow,
                    ) -> anyhow::Result<String> {
                        let mux = Mux::try_get().ok_or_else(|| anyhow::anyhow!("no mux"))?;
                        let pane = mux
                            .get_pane(pane_id)
                            .ok_or_else(|| anyhow::anyhow!("invalid pane {pane_id}"))?;
                        let lines = term_window.selection_lines(&pane);
                        lines_to_escapes(lines)
                    }
                    tx.try_send(do_it(pane_id, term_window).map_err(|err| format!("{err:#}")))
                        .ok();
                }));
                let result =
                    ask_window(&this.window, "get_selection_escapes_for_pane", notif, rx).await?;

                Ok(result)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims() -> ::window::Dimensions {
        ::window::Dimensions {
            pixel_width: 1920,
            pixel_height: 1080,
            dpi: 144,
        }
    }

    #[test]
    fn gui_window_dimensions_exposes_all_window_state_flags() {
        let payload = gui_window_dimensions(
            dims(),
            WindowState::FULL_SCREEN | WindowState::ALWAYS_ON_TOP | WindowState::HIDDEN,
        );

        assert_eq!(payload.pixel_width, 1920);
        assert_eq!(payload.pixel_height, 1080);
        assert_eq!(payload.dpi, 144);
        assert!(payload.is_full_screen);
        assert!(!payload.is_maximized);
        assert!(payload.is_hidden);
        assert!(payload.is_always_on_top);
        assert!(!payload.is_always_on_bottom);
        assert!(!payload.can_resize);
        assert!(!payload.can_paint);
        assert_eq!(
            payload.window_level,
            config::window::WindowLevel::AlwaysOnTop
        );
    }

    #[test]
    fn gui_window_dimensions_reports_normal_window_capabilities() {
        let payload = gui_window_dimensions(dims(), WindowState::default());

        assert!(!payload.is_full_screen);
        assert!(!payload.is_maximized);
        assert!(!payload.is_hidden);
        assert!(!payload.is_always_on_top);
        assert!(!payload.is_always_on_bottom);
        assert!(payload.can_resize);
        assert!(payload.can_paint);
        assert_eq!(payload.window_level, config::window::WindowLevel::Normal);
    }
}
