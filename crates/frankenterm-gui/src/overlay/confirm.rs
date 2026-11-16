use crate::scripting::guiwin::GuiWin;
use config::keyassignment::{Confirmation, KeyAssignment};
use mux::termwiztermtab::TermWizTerminal;
use mux_lua::MuxPane;
use std::rc::Rc;
use termwiz::cell::AttributeChange;
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, MouseButtons, MouseEvent};
use termwiz::surface::{Change, CursorVisibility, Position};
use termwiz::terminal::Terminal;

pub fn run_confirmation(message: &str, term: &mut TermWizTerminal) -> anyhow::Result<bool> {
    run_confirmation_impl(message, term)
}

fn run_confirmation_impl(message: &str, term: &mut TermWizTerminal) -> anyhow::Result<bool> {
    run_confirmation_controlled(message, term, None, || false)
}

pub(crate) fn run_confirmation_until(
    message: &str,
    term: &mut impl Terminal,
    deadline: std::time::Instant,
    mut still_authorized: impl FnMut() -> bool,
) -> anyhow::Result<bool> {
    run_confirmation_controlled(
        message,
        term,
        Some(std::time::Duration::from_millis(100)),
        || std::time::Instant::now() >= deadline || !still_authorized(),
    )
}

fn run_confirmation_controlled<T: Terminal>(
    message: &str,
    term: &mut T,
    poll_interval: Option<std::time::Duration>,
    mut cancelled: impl FnMut() -> bool,
) -> anyhow::Result<bool> {
    if cancelled() {
        return Ok(false);
    }
    term.set_raw_mode()?;

    let size = term.get_screen_size()?;
    anyhow::ensure!(
        size.cols >= 24 && size.rows >= 4,
        "confirmation viewport is too small"
    );

    // Render 80% wide, centered
    let text_width = size.cols.saturating_mul(80) / 100;
    let x_pos = size.cols.saturating_mul(10) / 100;

    // Fit text to the width
    let wrapped = textwrap::fill(message, text_width);

    let message_rows = wrapped.split("\n").count();
    anyhow::ensure!(
        message_rows + 2 <= size.rows,
        "confirmation does not fit the viewport"
    );
    // Now we want to vertically center the prompt in the view.
    // After the prompt there will be a blank line and then the "buttons",
    // so we add two to the number of rows.
    let top_row = (size.rows - (message_rows + 2)) / 2;

    let button_row = top_row + message_rows + 1;
    let mut active = ActiveButton::No;

    let yes_x = x_pos;
    let yes_w = 7;

    let no_x =  yes_x + yes_w + 8 /* spacer */;
    let no_w = 6;

    #[derive(Copy, Clone, PartialEq, Eq)]
    enum ActiveButton {
        None,
        Yes,
        No,
    }

    let render = |term: &mut T, active: ActiveButton| -> termwiz::Result<()> {
        let mut changes = vec![
            Change::ClearScreen(ColorAttribute::Default),
            Change::CursorVisibility(CursorVisibility::Hidden),
        ];

        for (y, row) in wrapped.split("\n").enumerate() {
            let row = row.trim_end();
            changes.push(Change::CursorPosition {
                x: Position::Absolute(x_pos),
                y: Position::Absolute(top_row + y),
            });
            changes.push(Change::Text(row.to_string()));
        }

        changes.push(Change::CursorPosition {
            x: Position::Absolute(x_pos),
            y: Position::Absolute(button_row),
        });

        if active == ActiveButton::Yes {
            changes.push(AttributeChange::Reverse(true).into());
        }
        changes.push(" [Y]es ".into());
        if active == ActiveButton::Yes {
            changes.push(AttributeChange::Reverse(false).into());
        }

        changes.push("        ".into());

        if active == ActiveButton::No {
            changes.push(AttributeChange::Reverse(true).into());
        }
        changes.push(" [N]o ".into());
        if active == ActiveButton::No {
            changes.push(AttributeChange::Reverse(false).into());
        }

        term.render(&changes)?;
        term.flush()
    };

    render(term, active)?;

    loop {
        if cancelled() {
            return Ok(false);
        }
        let event = match term.poll_input(poll_interval)? {
            Some(event) => event,
            None if poll_interval.is_some() => continue,
            None => return Ok(false),
        };
        if cancelled() {
            return Ok(false);
        }
        match event {
            InputEvent::Key(KeyEvent {
                key: KeyCode::Tab, ..
            }) => {
                active = if active == ActiveButton::Yes {
                    ActiveButton::No
                } else {
                    ActiveButton::Yes
                };
            }
            InputEvent::Key(KeyEvent {
                key: KeyCode::Enter,
                ..
            }) => {
                return Ok(active == ActiveButton::Yes);
            }
            InputEvent::Key(KeyEvent {
                key: KeyCode::Char('y' | 'Y'),
                ..
            }) => {
                return Ok(true);
            }
            InputEvent::Key(KeyEvent {
                key: KeyCode::Char('n' | 'N'),
                ..
            })
            | InputEvent::Key(KeyEvent {
                key: KeyCode::Escape,
                ..
            }) => {
                return Ok(false);
            }
            InputEvent::Mouse(MouseEvent {
                x,
                y,
                mouse_buttons,
                ..
            }) => {
                let x = x as usize;
                let y = y as usize;
                if y == button_row && x >= yes_x && x < yes_x + yes_w {
                    active = ActiveButton::Yes;
                    if mouse_buttons == MouseButtons::LEFT {
                        return Ok(true);
                    }
                } else if y == button_row && x >= no_x && x < no_x + no_w {
                    active = ActiveButton::No;
                    if mouse_buttons == MouseButtons::LEFT {
                        return Ok(false);
                    }
                } else {
                    active = ActiveButton::None;
                }

                if mouse_buttons != MouseButtons::NONE {
                    // Treat any other mouse button as cancel
                    return Ok(false);
                }
            }
            // Geometry changes invalidate the drawn button coordinates. An
            // expiring consent prompt must be reissued for the new viewport.
            InputEvent::Resized { .. } => return Ok(false),
            _ => continue,
        }

        render(term, active)?;
    }
}

#[cfg(test)]
mod osc52_confirmation_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use termwiz::terminal::{ScreenSize, TerminalWaker};

    // This is an explicit headless input/render sink for the production
    // confirmation controller, not a native window/accessibility producer.
    struct HeadlessConfirmationTerminal {
        input: VecDeque<InputEvent>,
        rendered: Vec<Change>,
        size: ScreenSize,
        polls: usize,
        revoke_on_poll: Option<Arc<AtomicBool>>,
        fail_render: bool,
    }

    impl HeadlessConfirmationTerminal {
        fn keys(keys: &[KeyCode]) -> Self {
            Self {
                input: keys
                    .iter()
                    .cloned()
                    .map(|key| {
                        InputEvent::Key(KeyEvent {
                            key,
                            modifiers: termwiz::input::Modifiers::NONE,
                        })
                    })
                    .collect(),
                rendered: Vec::new(),
                size: ScreenSize {
                    rows: 24,
                    cols: 80,
                    xpixel: 0,
                    ypixel: 0,
                },
                polls: 0,
                revoke_on_poll: None,
                fail_render: false,
            }
        }
    }

    impl Terminal for HeadlessConfirmationTerminal {
        fn set_raw_mode(&mut self) -> termwiz::Result<()> {
            Ok(())
        }
        fn set_cooked_mode(&mut self) -> termwiz::Result<()> {
            Ok(())
        }
        fn enter_alternate_screen(&mut self) -> termwiz::Result<()> {
            Ok(())
        }
        fn exit_alternate_screen(&mut self) -> termwiz::Result<()> {
            Ok(())
        }
        fn get_screen_size(&mut self) -> termwiz::Result<ScreenSize> {
            Ok(self.size)
        }
        fn set_screen_size(&mut self, size: ScreenSize) -> termwiz::Result<()> {
            self.size = size;
            Ok(())
        }
        fn render(&mut self, changes: &[Change]) -> termwiz::Result<()> {
            if self.fail_render {
                termwiz::bail!("planted render failure");
            }
            self.rendered.extend_from_slice(changes);
            Ok(())
        }
        fn flush(&mut self) -> termwiz::Result<()> {
            Ok(())
        }
        fn poll_input(&mut self, wait: Option<Duration>) -> termwiz::Result<Option<InputEvent>> {
            assert_eq!(
                wait,
                Some(Duration::from_millis(100)),
                "consent input wait must be bounded"
            );
            self.polls += 1;
            if let Some(authority) = &self.revoke_on_poll {
                authority.store(false, Ordering::Release);
            }
            self.input
                .pop_front()
                .map(Some)
                .ok_or_else(|| termwiz::error::Error::from("planted input disconnect".to_string()))
        }
        fn waker(&self) -> TerminalWaker {
            TerminalWaker::noop()
        }
    }

    #[test]
    fn osc52_confirmation_controller_requires_explicit_grant_and_renders_choices() {
        for (keys, expected) in [
            (vec![KeyCode::Enter], false),
            (vec![KeyCode::Char('y')], true),
            (vec![KeyCode::Tab, KeyCode::Enter], true),
            (vec![KeyCode::Char('n')], false),
            (vec![KeyCode::Escape], false),
        ] {
            let mut term = HeadlessConfirmationTerminal::keys(&keys);
            let granted = run_confirmation_until(
                "Public consent test: allow once?",
                &mut term,
                Instant::now() + Duration::from_secs(1),
                || true,
            )
            .unwrap();
            assert_eq!(granted, expected, "keys={keys:?}");
            let text = term
                .rendered
                .iter()
                .filter_map(|change| match change {
                    Change::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert!(text.contains("Public consent test: allow once?"));
            assert!(text.contains("[Y]es") && text.contains("[N]o"));
            assert_eq!(term.polls, keys.len());
        }
        println!(
            "OSC52_CONFIRMATION headless_controller default_no=true explicit_yes_and_tab_enter=true rendered_choices=true native_evidence=unproven"
        );
    }

    #[test]
    fn osc52_confirmation_controller_rejects_expired_revoked_unrendered_and_resized_grants() {
        let mut expired = HeadlessConfirmationTerminal::keys(&[KeyCode::Char('y')]);
        assert!(!run_confirmation_until("expired", &mut expired, Instant::now(), || true).unwrap());
        assert_eq!(expired.polls, 0);
        assert!(expired.rendered.is_empty());
        let authority = Arc::new(AtomicBool::new(true));
        let mut revoked = HeadlessConfirmationTerminal::keys(&[KeyCode::Char('y')]);
        revoked.revoke_on_poll = Some(Arc::clone(&authority));
        assert!(
            !run_confirmation_until(
                "revoked",
                &mut revoked,
                Instant::now() + Duration::from_secs(1),
                || authority.load(Ordering::Acquire)
            )
            .unwrap()
        );
        assert_eq!(
            revoked.polls, 1,
            "revocation must win over the already-read affirmative key"
        );
        let mut failed = HeadlessConfirmationTerminal::keys(&[KeyCode::Char('y')]);
        failed.fail_render = true;
        assert!(
            run_confirmation_until(
                "unrendered",
                &mut failed,
                Instant::now() + Duration::from_secs(1),
                || true
            )
            .is_err()
        );
        assert_eq!(failed.polls, 0);
        let mut resized = HeadlessConfirmationTerminal::keys(&[KeyCode::Char('y')]);
        resized
            .input
            .push_front(InputEvent::Resized { rows: 1, cols: 1 });
        assert!(
            !run_confirmation_until(
                "resized",
                &mut resized,
                Instant::now() + Duration::from_secs(1),
                || true
            )
            .unwrap()
        );
        let mut too_small = HeadlessConfirmationTerminal::keys(&[KeyCode::Char('y')]);
        too_small.size.rows = 1;
        assert!(
            run_confirmation_until(
                "small",
                &mut too_small,
                Instant::now() + Duration::from_secs(1),
                || true
            )
            .is_err()
        );
        assert_eq!(too_small.polls, 0);
        println!("OSC52_CONFIRMATION expired_revoked_failed_render_resized_small_zero_grants=true");
    }
}

pub fn show_confirmation_overlay(
    mut term: TermWizTerminal,
    args: Confirmation,
    window: GuiWin,
    pane: MuxPane,
) -> anyhow::Result<()> {
    let name = match *args.action {
        KeyAssignment::EmitEvent(id) => id,
        _ => anyhow::bail!("Confirmation requires action to be defined by wezterm.action_callback"),
    };

    if let Ok(confirm) = run_confirmation_impl(&args.message, &mut term) {
        if confirm {
            super::reserve_overlay_main_thread(
                promise::spawn::MainThreadServiceClass::Input,
                super::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
                "confirmation action",
            )?
            .spawn(async move {
                trampoline(name, window, pane);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
        } else if let Some(key_assignment) = args.cancel {
            if let KeyAssignment::EmitEvent(id) = *key_assignment {
                super::reserve_overlay_main_thread(
                    promise::spawn::MainThreadServiceClass::Input,
                    super::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
                    "confirmation cancellation",
                )?
                .spawn(async move {
                    trampoline(id, window, pane);
                    anyhow::Result::<()>::Ok(())
                })
                .detach();
            }
        }
    }
    Ok(())
}

fn trampoline(name: String, window: GuiWin, pane: MuxPane) {
    if let Ok(reservation) = super::reserve_overlay_main_thread(
        promise::spawn::MainThreadServiceClass::Input,
        super::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
        "confirmation callback",
    ) {
        reservation
            .spawn_local(async move {
                config::with_lua_config_on_main_thread(move |lua| do_event(lua, name, window, pane))
                    .await
            })
            .detach();
    }
}

async fn do_event(
    lua: Option<Rc<mlua::Lua>>,
    name: String,
    window: GuiWin,
    pane: MuxPane,
) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi((window, pane))?;

        if let Err(err) = config::lua::emit_event(lua.as_ref().clone(), (name.clone(), args)).await
        {
            log::error!("while processing {} event: {:#}", name, err);
        }
    }

    Ok(())
}
