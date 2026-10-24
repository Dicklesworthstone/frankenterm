//! ft-utjx0: `ft tui` draws its first frame on a real terminal and quits on
//! `q`. The ftui backend's tests drive the model against an in-memory frame,
//! so nothing else launches the TUI on a PTY: builds whose ftui had no
//! terminal backend exited here with FT-9001 before drawing anything.
#![cfg(unix)]

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";
const TITLE: &str = "FrankenTerm Control Center";

/// `bytes` without escape sequences (CSI, OSC and two-byte escapes), so text
/// split by styling still reads as one string.
fn visible_text(bytes: &[u8]) -> String {
    let mut text = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            text.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'[') => {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                    i += 1;
                }
                i += if bytes.get(i) == Some(&0x1b) { 2 } else { 1 };
            }
            _ => i += 2,
        }
    }
    String::from_utf8_lossy(&text).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[test]
fn ft_tui_paints_its_first_frame_and_quits_on_q() {
    let home = tempfile::tempdir().expect("scratch HOME");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 110,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_ft"));
    cmd.args(["-v", "tui"]);
    cmd.env_clear();
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd.env("XDG_DATA_HOME", home.path().join(".local/share"));
    cmd.env("XDG_RUNTIME_DIR", home.path().join("run"));
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("TERM", "xterm-256color");
    cmd.cwd(home.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn ft tui");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut writer = pair.master.take_writer().expect("pty writer");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut out = Vec::new();
    let painted = |out: &[u8]| {
        find(out, ENTER_ALT_SCREEN).is_some_and(|at| visible_text(&out[at..]).contains(TITLE))
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !painted(&out) {
        if let Some(status) = child.try_wait().expect("poll ft tui") {
            panic!(
                "ft tui exited before its first frame ({status:?}):\n{}",
                visible_text(&out)
            );
        }
        assert!(
            Instant::now() < deadline,
            "no frame within 60 s:\n{}",
            visible_text(&out)
        );
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            out.extend(chunk);
        }
    }

    writer.write_all(b"q").expect("send q");
    writer.flush().expect("flush q");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll ft tui") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("ft tui did not quit on q:\n{}", visible_text(&out));
        }
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            out.extend(chunk);
        }
    };
    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(300)) {
        out.extend(chunk);
    }
    assert!(status.success(), "ft tui exit status {status:?}");

    let entered = find(&out, ENTER_ALT_SCREEN).expect("entered the alternate screen");
    let left =
        entered + find(&out[entered..], LEAVE_ALT_SCREEN).expect("left the alternate screen");
    // `-v` logs at debug; while the TUI owns the terminal no log line may be
    // drawn into its frame. A log line reads `<RFC 3339 time>Z  INFO ...`.
    let frame = visible_text(&out[entered..left]);
    for level in ["Z TRACE ", "Z DEBUG ", "Z  INFO ", "Z  WARN ", "Z ERROR "] {
        assert!(
            !frame.contains(level),
            "a log line was drawn inside the TUI frame:\n{frame}"
        );
    }
}
