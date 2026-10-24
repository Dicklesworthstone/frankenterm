//! `frankenterm-mobile-attach [--socket PATH] [--pane ID]`
//!
//! Run by the FrankenTerm iPhone app over an SSH PTY: attaches the phone's
//! terminal to one pane of this machine's running FrankenTerm mux server.
//! See the library documentation for the private DCS messages.

use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    attach::main()
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("frankenterm-mobile-attach: only unix hosts run a FrankenTerm mux socket");
    ExitCode::from(2)
}

#[cfg(unix)]
mod attach {
    use std::ffi::OsString;
    use std::io::{self, Read, Write};
    use std::panic::AssertUnwindSafe;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::mpsc::{self, Sender};

    use frankenterm_mobile_attach::default_socket_path;
    use frankenterm_mobile_attach::session::{Event, PtySize, SessionConfig, run_session};
    use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr};
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
    use signal_hook::iterator::Signals;

    const USAGE: &str = "\
usage: frankenterm-mobile-attach [--socket PATH] [--pane ID]

Attach this terminal (normally an SSH PTY from the FrankenTerm iPhone app)
to one pane of this machine's running FrankenTerm mux server.

  --socket PATH  mux socket (default: $FRANKENTERM_UNIX_SOCKET, else
                 $XDG_RUNTIME_DIR/frankenterm/sock)
  --pane ID      pane to attach first (default: the pane the desktop
                 most recently used)
  -h, --help     show this help
  -V, --version  show the version";

    struct Args {
        socket: Option<PathBuf>,
        pane: Option<u64>,
    }

    enum Parsed {
        Run(Args),
        Exit(ExitCode),
    }

    fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, String> {
        let mut parsed = Args {
            socket: None,
            pane: None,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let arg = arg
                .into_string()
                .map_err(|arg| format!("unexpected argument {}", arg.to_string_lossy()))?;
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name.to_string(), Some(value)),
                _ => (arg.clone(), None),
            };
            let mut value = |name: &str| -> Result<OsString, String> {
                inline
                    .map(OsString::from)
                    .or_else(|| args.next())
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match name.as_str() {
                "--socket" => parsed.socket = Some(PathBuf::from(value("--socket")?)),
                "--pane" => {
                    let id = value("--pane")?;
                    let id = id
                        .to_str()
                        .and_then(|id| id.parse::<u64>().ok())
                        .ok_or_else(|| {
                            format!("--pane needs a pane id, got {}", id.to_string_lossy())
                        })?;
                    parsed.pane = Some(id);
                }
                "-h" | "--help" => {
                    println!("{USAGE}");
                    return Ok(Parsed::Exit(ExitCode::SUCCESS));
                }
                "-V" | "--version" => {
                    println!("frankenterm-mobile-attach {}", env!("CARGO_PKG_VERSION"));
                    return Ok(Parsed::Exit(ExitCode::SUCCESS));
                }
                other => return Err(format!("unexpected argument {other}")),
            }
        }
        Ok(Parsed::Run(parsed))
    }

    /// The PTY's size; an unknown size reads as 80x24.
    fn window_size() -> PtySize {
        match tcgetwinsize(io::stdout()) {
            Ok(size) if size.ws_row > 0 && size.ws_col > 0 => PtySize {
                rows: size.ws_row,
                cols: size.ws_col,
                pixel_width: size.ws_xpixel,
                pixel_height: size.ws_ypixel,
            },
            _ => PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        }
    }

    /// Raw mode on the PTY for the guard's lifetime; dropping it (also while
    /// unwinding) restores the original settings.
    struct RawMode {
        original: Termios,
    }

    impl RawMode {
        fn enable(original: Termios) -> io::Result<Self> {
            let mut raw = original.clone();
            raw.make_raw();
            // TCSANOW: never discard keystrokes typed during startup.
            tcsetattr(io::stdin(), OptionalActions::Now, &raw)?;
            Ok(Self { original })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = tcsetattr(io::stdin(), OptionalActions::Now, &self.original);
        }
    }

    fn spawn_stdin_reader(events: Sender<Event>) -> io::Result<()> {
        std::thread::Builder::new()
            .name("mobile-attach-stdin".to_string())
            .spawn(move || {
                let mut stdin = io::stdin().lock();
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(count) => {
                            if events.send(Event::Input(buf[..count].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        // EIO: the SSH channel (PTY master) is gone.
                        Err(_) => break,
                    }
                }
                let _ = events.send(Event::InputClosed);
            })?;
        Ok(())
    }

    fn spawn_signal_watcher(events: Sender<Event>, original: Termios) -> io::Result<()> {
        let mut signals = Signals::new([SIGWINCH, SIGHUP, SIGTERM, SIGINT])?;
        std::thread::Builder::new()
            .name("mobile-attach-signals".to_string())
            .spawn(move || {
                let mut terminating = false;
                for signal in signals.forever() {
                    if signal == SIGWINCH {
                        let _ = events.send(Event::Resize(window_size()));
                        continue;
                    }
                    if terminating {
                        // Asked twice while still detaching: stop now.
                        let _ = tcsetattr(io::stdin(), OptionalActions::Now, &original);
                        std::process::exit(1);
                    }
                    terminating = true;
                    let _ = events.send(Event::Terminate);
                }
            })?;
        Ok(())
    }

    pub fn main() -> ExitCode {
        let args = match parse_args(std::env::args_os().skip(1)) {
            Ok(Parsed::Run(args)) => args,
            Ok(Parsed::Exit(code)) => return code,
            Err(message) => {
                eprintln!("frankenterm-mobile-attach: {message}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        };
        if !isatty(io::stdin()) || !isatty(io::stdout()) {
            eprintln!(
                "frankenterm-mobile-attach: stdin and stdout must be a terminal (run it with `ssh -t`)"
            );
            return ExitCode::from(2);
        }
        let socket = args.socket.unwrap_or_else(default_socket_path);
        if !socket.exists() {
            eprintln!(
                "frankenterm-mobile-attach: no FrankenTerm mux socket at {} \
                 (is frankenterm-mux-server running? pass --socket)",
                socket.display()
            );
            return ExitCode::FAILURE;
        }
        let original = match tcgetattr(io::stdin()) {
            Ok(original) => original,
            Err(error) => {
                eprintln!("frankenterm-mobile-attach: cannot read terminal settings: {error}");
                return ExitCode::FAILURE;
            }
        };

        let (sender, events) = mpsc::channel();
        if let Err(error) = spawn_signal_watcher(sender.clone(), original.clone()) {
            eprintln!("frankenterm-mobile-attach: cannot watch signals: {error}");
            return ExitCode::FAILURE;
        }
        let raw = match RawMode::enable(original) {
            Ok(raw) => raw,
            Err(error) => {
                eprintln!("frankenterm-mobile-attach: cannot enter raw mode: {error}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(error) = spawn_stdin_reader(sender) {
            drop(raw);
            eprintln!("frankenterm-mobile-attach: cannot read input: {error}");
            return ExitCode::FAILURE;
        }

        let config = SessionConfig::new(socket, args.pane, window_size());
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let result =
            std::panic::catch_unwind(AssertUnwindSafe(|| run_session(&config, &events, &mut out)));
        if result.is_err() {
            // The session could not reset the phone's terminal modes itself.
            let _ = out.write_all(b"\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[?25h\x1b[0 q\x1b[?2004l\r\n");
            let _ = out.flush();
        }
        drop(out);
        drop(raw);
        match result {
            Ok(Ok(_)) => ExitCode::SUCCESS,
            Ok(Err(error)) => {
                eprintln!("frankenterm-mobile-attach: {error}");
                ExitCode::FAILURE
            }
            Err(_) => {
                eprintln!("frankenterm-mobile-attach: internal error; the terminal was restored");
                ExitCode::from(101)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn parse(args: &[&str]) -> Result<Parsed, String> {
            parse_args(args.iter().map(OsString::from))
        }

        #[test]
        fn flags_take_separate_or_inline_values() {
            let Ok(Parsed::Run(args)) = parse(&["--socket", "/run/s", "--pane=7"]) else {
                panic!("expected a run");
            };
            assert_eq!(args.socket, Some(PathBuf::from("/run/s")));
            assert_eq!(args.pane, Some(7));
            let Ok(Parsed::Run(args)) = parse(&[]) else {
                panic!("expected a run");
            };
            assert!(args.socket.is_none() && args.pane.is_none());
        }

        #[test]
        fn bad_arguments_are_rejected() {
            assert!(parse(&["--pane", "x"]).is_err());
            assert!(parse(&["--pane"]).is_err());
            assert!(parse(&["--bogus"]).is_err());
            assert!(parse(&["extra"]).is_err());
        }
    }
}
