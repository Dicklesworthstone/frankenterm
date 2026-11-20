//! The Windows GUI's built-in look. The Mac app ships its default config and
//! the Pragmasevka faces in `Contents/Resources`; the Windows package is a set
//! of executables, so `frankenterm-gui.exe` embeds both and writes them once
//! per user under `%LOCALAPPDATA%\FrankenTerm\bundled\<digest>\`. The config
//! crate then searches that directory like the Mac bundle's resources: after
//! every user config, with its `fonts` directory ahead of the system fonts.

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG: &str = include_str!("../frankenterm.lua");
const FONT_PAYLOAD_ZST: &[u8] = include_bytes!("../../frankenterm/assets/Pragmasevka_NF.zip.zst");
const FONT_FILES: &[&str] = &[
    "pragmasevka-nf-regular.ttf",
    "pragmasevka-nf-bold.ttf",
    "pragmasevka-nf-italic.ttf",
    "pragmasevka-nf-bolditalic.ttf",
];
/// Written last: a directory without it was not finished.
const COMPLETE: &str = ".complete";

/// Unpacks the built-in look (at most once per user and content) and hands
/// its directory to the config crate. Failure only costs the look: the stock
/// defaults still apply, so it is logged rather than fatal.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn install() {
    let Some(local) = dirs_next::data_local_dir() else {
        log::warn!("FrankenTerm's built-in look is unavailable: no local data directory");
        return;
    };
    match unpack(&local.join("FrankenTerm").join("bundled")) {
        Ok(dir) => config::set_bundled_resources_dir(dir),
        Err(err) => log::warn!("FrankenTerm's built-in look is unavailable: {err:#}"),
    }
}

/// The name of this build's directory: a digest of what it holds.
fn bundle_name() -> String {
    let mut digest = Sha256::new();
    digest.update(DEFAULT_CONFIG.as_bytes());
    digest.update(FONT_PAYLOAD_ZST);
    digest.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A finished directory for this build's config and fonts under `base`,
/// written if there is none yet.
///
/// Several GUI processes may start at once, and one may already be reading
/// fonts from a directory while another starts. So a directory only ever
/// appears by renaming a finished staging copy into place, marker included,
/// and none is ever deleted or rewritten here: no process can remove or
/// change a copy another one uses.
fn unpack(base: &Path) -> anyhow::Result<PathBuf> {
    let name = bundle_name();
    if let Some(dir) = finished(base, &name) {
        return Ok(dir);
    }
    // Only this process uses this name, so a leftover is its own.
    let staging = base.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clearing {}", staging.display()))?;
    }
    let installed = write_bundle(&staging).and_then(|()| install_staged(&staging, base, &name));
    if installed.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    installed
}

/// A finished copy of the bundle called `name`: the directory of that name,
/// else the first finished copy installed beside it (see [`install_staged`]).
fn finished(base: &Path, name: &str) -> Option<PathBuf> {
    let dir = base.join(name);
    if dir.join(COMPLETE).is_file() {
        return Some(dir);
    }
    // Several may exist when processes started together; any is complete,
    // and taking the least name makes every process pick the same one.
    let prefix = format!("{name}-");
    std::fs::read_dir(base)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .filter(|copy| copy.join(COMPLETE).is_file())
        .min()
}

/// Moves the finished `staging` directory into place as `name` and returns
/// the directory to use. When another process installed `name` first, its
/// copy is used and this one dropped. When the name is held by a directory
/// without the marker, which only a change from outside leaves, that
/// directory is left alone and this copy goes beside it as `name-<pid>`.
fn install_staged(staging: &Path, base: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let dir = base.join(name);
    let Err(err) = std::fs::rename(staging, &dir) else {
        return Ok(dir);
    };
    if dir.join(COMPLETE).is_file() {
        // Only this process's own copy is deleted; it costs disk space only.
        let _ = std::fs::remove_dir_all(staging);
        return Ok(dir);
    }
    if !dir.exists() {
        return Err(err).with_context(|| format!("installing {}", dir.display()));
    }
    let beside = base.join(format!("{name}-{}", std::process::id()));
    std::fs::rename(staging, &beside)
        .with_context(|| format!("installing {}", beside.display()))?;
    Ok(beside)
}

fn write_bundle(dir: &Path) -> anyhow::Result<()> {
    let fonts = dir.join("fonts");
    std::fs::create_dir_all(&fonts).with_context(|| format!("creating {}", fonts.display()))?;
    std::fs::write(dir.join("frankenterm.lua"), DEFAULT_CONFIG)
        .context("writing frankenterm.lua")?;
    let archive =
        zstd::stream::decode_all(FONT_PAYLOAD_ZST).context("decompressing the font payload")?;
    let mut archive = tar::Archive::new(archive.as_slice());
    let mut written = 0;
    for entry in archive.entries().context("reading the font payload")? {
        let mut entry = entry.context("reading the font payload")?;
        let path = entry
            .path()
            .context("reading the font payload")?
            .into_owned();
        // Only the four named faces, at the archive's top level.
        let Some(name) = path.to_str().filter(|name| FONT_FILES.contains(name)) else {
            continue;
        };
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .with_context(|| format!("reading {name}"))?;
        std::fs::write(fonts.join(name), data).with_context(|| format!("writing {name}"))?;
        written += 1;
    }
    anyhow::ensure!(
        written == FONT_FILES.len(),
        "the font payload held {written} of {} faces",
        FONT_FILES.len()
    );
    std::fs::write(dir.join(COMPLETE), b"").context("finishing the bundle")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_the_config_and_all_four_faces_once() {
        let base = tempfile::tempdir().unwrap();
        let dir = unpack(base.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("frankenterm.lua")).unwrap(),
            DEFAULT_CONFIG
        );
        for face in FONT_FILES {
            let data = std::fs::read(dir.join("fonts").join(face)).unwrap();
            assert!(data.len() > 1_000_000, "{face} is a whole face");
        }
        assert!(dir.join(COMPLETE).is_file());

        // A finished directory is reused as it is.
        let marker = dir.join("fonts").join(FONT_FILES[0]);
        let before = std::fs::metadata(&marker).unwrap().modified().unwrap();
        assert_eq!(unpack(base.path()).unwrap(), dir);
        assert_eq!(
            std::fs::metadata(&marker).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn a_directory_another_process_installed_first_is_kept() {
        let base = tempfile::tempdir().unwrap();
        let dir = unpack(base.path()).unwrap();
        // The first process is using its directory.
        let in_use = dir.join("in-use");
        std::fs::write(&in_use, b"").unwrap();

        // A second process found no marker just before the first one's
        // rename, wrote its own copy, and now installs it.
        let staging = base.path().join(".staging-second");
        write_bundle(&staging).unwrap();
        let used = install_staged(&staging, base.path(), &bundle_name()).unwrap();

        assert_eq!(
            used, dir,
            "the second process uses the first one's directory"
        );
        assert!(in_use.is_file(), "the first directory is kept");
        assert!(dir.join(COMPLETE).is_file());
        assert!(!staging.exists(), "the second copy is cleared");
    }

    #[test]
    fn an_unfinished_directory_is_left_alone_and_a_copy_installed_beside_it() {
        let base = tempfile::tempdir().unwrap();
        let dir = unpack(base.path()).unwrap();
        // Changed from outside: its marker and one face are gone, and a
        // process may still be reading the faces that are left.
        std::fs::remove_file(dir.join(COMPLETE)).unwrap();
        std::fs::remove_file(dir.join("fonts").join(FONT_FILES[1])).unwrap();

        let copy = unpack(base.path()).unwrap();
        assert_eq!(
            copy,
            base.path()
                .join(format!("{}-{}", bundle_name(), std::process::id()))
        );
        assert!(copy.join(COMPLETE).is_file());
        for face in FONT_FILES {
            assert!(copy.join("fonts").join(face).is_file(), "{face}");
        }
        assert!(
            dir.join("fonts").join(FONT_FILES[0]).is_file(),
            "the unfinished directory is not touched"
        );
        assert!(!dir.join(COMPLETE).exists());

        // Later starts use that copy as it is.
        assert_eq!(unpack(base.path()).unwrap(), copy);
        let mut entries: Vec<_> = std::fs::read_dir(base.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        assert_eq!(entries, [dir, copy], "no staging directory is left behind");
    }
}
