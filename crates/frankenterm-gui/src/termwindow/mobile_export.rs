//! Deliberate GUI-to-companion transfer over a private inherited pipe. This
//! channel never publishes a file, opens a listener, or invokes a shell.

use anyhow::{Context, ensure};
use filedescriptor::FileDescriptor;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static EXPORT_OPEN: AtomicBool = AtomicBool::new(false);

struct ExportOwner;
impl Drop for ExportOwner {
    fn drop(&mut self) {
        EXPORT_OPEN.store(false, Ordering::Release);
    }
}

pub(super) fn start_export(config: config::ConfigHandle) -> anyhow::Result<()> {
    ensure!(
        !EXPORT_OPEN.swap(true, Ordering::AcqRel),
        "A desktop export is already open. Close that export window before capturing another snapshot."
    );
    let owner = ExportOwner;
    std::thread::Builder::new()
        .name("ft-mobile-export".into())
        .spawn(move || {
            let _owner = owner;
            if let Err(error) = export(config) {
                config::show_error(&format!("Export to iPhone: {error:#}"));
            }
        })
        .context("Cannot start the desktop export worker")?;
    Ok(())
}

fn companion_executable() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let mut candidates = Vec::new();
    if let Some(macos) = current.parent() {
        // A bundled companion may live inside the full desktop app; development
        // builds can place the two application bundles next to one another.
        if let Some(contents) = macos.parent() {
            candidates.push(
                contents.join("Helpers/FrankenTermExport.app/Contents/MacOS/FrankenTermExport"),
            );
            if let Some(apps) = contents.parent().and_then(|app| app.parent()) {
                candidates
                    .push(apps.join("FrankenTermExport.app/Contents/MacOS/FrankenTermExport"));
            }
        }
    }
    candidates.push(
        config::HOME_DIR
            .join("Applications/FrankenTermExport.app/Contents/MacOS/FrankenTermExport"),
    );
    candidates.push(PathBuf::from(
        "/Applications/FrankenTermExport.app/Contents/MacOS/FrankenTermExport",
    ));
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| anyhow::anyhow!(
        "Install FrankenTermExport.app beside FrankenTerm.app or in Applications, then choose Export to iPhone again."
    ))
}

fn export(config: config::ConfigHandle) -> anyhow::Result<()> {
    let source = config::mobile_export::running_companion_source(&config)?;
    let bytes = serde_json::to_vec(&source)?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "The desktop snapshot is too large"
    );
    ensure!(
        config::configuration().generation() == config.generation(),
        "The desktop configuration changed while its public SSH sources were collected. Capture a new snapshot."
    );
    let mut child = Command::new(companion_executable()?)
        .arg("--desktop-source-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Cannot open the native export companion")?;
    let receipt = config::mobile_export::desktop_snapshot_receipt(&bytes);
    let delivery = deliver(&mut child, &bytes, Duration::from_secs(15))
        .and_then(|()| confirm_source(&mut child, receipt.as_bytes(), Duration::from_secs(15)));
    if delivery.is_err() {
        // This is the exact child we started, still unreaped. A partial source
        // is never retried against a different process or a static inventory.
        let _ = child.kill();
    }
    // One worker owns one companion, including reaping it. In handoff mode the
    // companion exits when its last window closes; the global admission stays
    // held until then, so repeated menu actions cannot accumulate processes.
    let reaped = child.wait();
    delivery?;
    reaped.context("Cannot reap the native export companion")?;
    Ok(())
}

fn deliver(child: &mut Child, bytes: &[u8], budget: Duration) -> anyhow::Result<()> {
    let input = child
        .stdin
        .take()
        .context("The companion has no input pipe")?;
    let mut pipe = FileDescriptor::dup(&input)?;
    drop(input);
    pipe.set_non_blocking(true)?;
    let deadline = Instant::now() + budget;
    let mut offset = 0;
    while offset < bytes.len() {
        ensure!(
            Instant::now() < deadline,
            "The companion did not accept the desktop snapshot in time"
        );
        ensure!(
            child.try_wait()?.is_none(),
            "The companion exited before accepting its desktop snapshot"
        );
        match pipe.write(&bytes[offset..]) {
            Ok(0) => anyhow::bail!("The companion closed its desktop snapshot input"),
            Ok(count) => offset += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                // This dedicated blocking worker has one pipe. A short bounded
                // backoff also avoids macOS select's descriptor-number ceiling
                // when the desktop already owns a large fleet of PTYs.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    // EOF, rather than a second mutable control message, commits this exact
    // bounded snapshot to the companion's strict parser.
    Ok(())
}

fn confirm_source(child: &mut Child, expected: &[u8], budget: Duration) -> anyhow::Result<()> {
    let output = child
        .stdout
        .take()
        .context("The companion has no acknowledgement pipe")?;
    let mut pipe = FileDescriptor::dup(&output)?;
    drop(output);
    pipe.set_non_blocking(true)?;
    let deadline = Instant::now() + budget;
    let mut receipt = Vec::with_capacity(expected.len());
    let mut buffer = [0_u8; 128];
    loop {
        ensure!(
            Instant::now() < deadline,
            "The installed companion did not acknowledge this desktop snapshot. Install the matching FrankenTermExport build."
        );
        match pipe.read(&mut buffer) {
            Ok(0) => anyhow::bail!("The companion closed without accepting this desktop snapshot"),
            Ok(count) => {
                ensure!(
                    count <= expected.len() - receipt.len(),
                    "Unexpected companion acknowledgement"
                );
                receipt.extend_from_slice(&buffer[..count]);
                ensure!(
                    expected.starts_with(&receipt),
                    "The companion acknowledged another desktop snapshot"
                );
                if receipt.len() == expected.len() {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                // Read the pipe through EOF rather than racing waitpid against
                // a valid final receipt that was written just before exit.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn private_pipe_preserves_large_snapshot_exactly_without_a_shell() {
        let expected = vec![b'x'; 512 * 1024];
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            output.take(512 * 1024 + 1).read_to_end(&mut bytes).unwrap();
            bytes
        });
        let result = deliver(&mut child, &expected, Duration::from_secs(3));
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        let actual = reader.join().unwrap();
        result.unwrap();
        assert!(status.success());
        assert_eq!(actual, expected);
    }

    #[test]
    fn unresponsive_companion_cannot_hold_snapshot_delivery_forever() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let result = deliver(
            &mut child,
            &vec![b'x'; 512 * 1024],
            Duration::from_millis(50),
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn companion_must_acknowledge_the_exact_consumed_source() {
        let expected = config::mobile_export::desktop_snapshot_receipt(b"one immutable source");
        for (actual, accepted) in [
            (expected.clone(), true),
            (
                config::mobile_export::desktop_snapshot_receipt(b"a different source"),
                false,
            ),
            ("old companion output\n".into(), false),
        ] {
            let mut child = Command::new("/usr/bin/printf")
                .args(["%s", &actual])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let result = confirm_source(&mut child, expected.as_bytes(), Duration::from_secs(2));
            let _ = child.wait();
            assert_eq!(result.is_ok(), accepted);
        }
    }

    #[test]
    fn an_old_companion_that_ignores_the_handoff_flag_is_refused() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let receipt = config::mobile_export::desktop_snapshot_receipt(b"small source fits in pipe");
        let result = confirm_source(&mut child, receipt.as_bytes(), Duration::from_millis(50));
        let _ = child.kill();
        let _ = child.wait();
        assert!(result.is_err());
    }
}
