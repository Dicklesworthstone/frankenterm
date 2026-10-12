//! The operating system's own symlinked top-level directories (ft-r937n).
//!
//! The no-follow directory walkers open every component of a path from `/`
//! without following a symlink, so that a link planted where they create
//! files cannot redirect a write. But macOS makes `/tmp`, `/var` and `/etc`
//! symlinks into `/private`, and ostree systems make `/home` one. A
//! workspace, data directory or temporary directory under any of them was
//! refused outright.
//!
//! [`resolve_system_symlinks`] rewrites a path through those links, and only
//! those: a symlink that `/` itself holds, where root owns both and nobody
//! else can write `/`. Nothing but root's own administration of the machine
//! puts a link there. Every other symlink stays in the path for the walker
//! to refuse, whoever owns it:
//! - one the user owns, because a cloned repository can carry a symlink;
//! - one root owns deeper in the tree, because root's ownership there says
//!   only who ran the command: a `sudo git clone`, an image `COPY` or a
//!   container's bind mount writes an attacker's link as root;
//! - one root owns in a sticky directory, because sticky stops others
//!   replacing an entry, not hard-linking one of root's links in under a
//!   name that is still free.

use std::borrow::Cow;
use std::path::Path;

/// Rewrites `path` through the system's symlinks among `/`'s own entries.
///
/// Best effort, and never an error: a path it cannot inspect, a relative
/// path and one with a `..` component come back unchanged. The no-follow
/// walk of the result remains the authority.
#[must_use]
pub fn resolve_system_symlinks(path: &Path) -> Cow<'_, Path> {
    #[cfg(unix)]
    {
        unix::resolve_with(path, &unix::inspect)
    }
    #[cfg(not(unix))]
    {
        Cow::Borrowed(path)
    }
}

#[cfg(unix)]
mod unix {
    use std::borrow::Cow;
    use std::path::{Component, Path, PathBuf};

    /// Links followed for one path. `/var` and `/tmp` take one each.
    const MAX_HOPS: usize = 40;

    /// What a path names, without following it.
    #[derive(Clone)]
    pub(super) struct Entry {
        pub(super) owner: u32,
        pub(super) mode: u32,
        /// Where it points, if it is a symlink.
        pub(super) link_target: Option<PathBuf>,
    }

    pub(super) fn inspect(path: &Path) -> Option<Entry> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::symlink_metadata(path).ok()?;
        let link_target = if metadata.file_type().is_symlink() {
            Some(std::fs::read_link(path).ok()?)
        } else {
            None
        };
        Some(Entry {
            owner: metadata.uid(),
            mode: metadata.mode(),
            link_target,
        })
    }

    pub(super) fn resolve_with<'a>(
        path: &'a Path,
        inspect: &dyn Fn(&Path) -> Option<Entry>,
    ) -> Cow<'a, Path> {
        if !path.is_absolute() || has_parent_component(path) {
            return Cow::Borrowed(path);
        }
        let mut resolved = Cow::Borrowed(path);
        for _ in 0..MAX_HOPS {
            let Some(next) = follow_root_symlink(&resolved, inspect) else {
                break;
            };
            resolved = Cow::Owned(next);
        }
        resolved
    }

    /// `path` with its first component replaced by that component's target,
    /// when the component is a symlink only root could have put in `/`.
    ///
    /// Only `/` and its one entry are inspected, and only root can change
    /// either, so there is nothing here for anyone else to race.
    fn follow_root_symlink(
        path: &Path,
        inspect: &dyn Fn(&Path) -> Option<Entry>,
    ) -> Option<PathBuf> {
        const GROUP_OR_OTHER_WRITE: u32 = 0o022;

        let mut components = path.components();
        let Some(Component::RootDir) = components.next() else {
            return None;
        };
        let Some(Component::Normal(name)) = components.next() else {
            return None;
        };
        let root = Path::new("/");
        let holder = inspect(root)?;
        let link = inspect(&root.join(name))?;
        let target = link.link_target.as_ref()?;
        let only_root_places_it =
            link.owner == 0 && holder.owner == 0 && holder.mode & GROUP_OR_OTHER_WRITE == 0;
        if !only_root_places_it || has_parent_component(target) {
            return None;
        }
        let mut followed = root.join(target);
        followed.extend(components);
        Some(followed)
    }

    fn has_parent_component(path: &Path) -> bool {
        path.components()
            .any(|component| matches!(component, Component::ParentDir))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;

        const USER: u32 = 501;

        #[derive(Default)]
        struct FakeFs(HashMap<PathBuf, Entry>);

        impl FakeFs {
            fn dir(mut self, path: &str, owner: u32, mode: u32) -> Self {
                self.0.insert(
                    PathBuf::from(path),
                    Entry {
                        owner,
                        mode,
                        link_target: None,
                    },
                );
                self
            }

            fn link(mut self, path: &str, owner: u32, target: &str) -> Self {
                self.0.insert(
                    PathBuf::from(path),
                    Entry {
                        owner,
                        mode: 0o120_755,
                        link_target: Some(PathBuf::from(target)),
                    },
                );
                self
            }

            fn resolve(&self, path: &str) -> PathBuf {
                resolve_with(Path::new(path), &|probe| self.0.get(probe).cloned()).into_owned()
            }
        }

        /// `/` and macOS's links from it into `/private`.
        fn macos() -> FakeFs {
            FakeFs::default()
                .dir("/", 0, 0o755)
                .link("/var", 0, "private/var")
                .link("/tmp", 0, "private/tmp")
                .dir("/private", 0, 0o755)
                .dir("/Users", 0, 0o755)
        }

        #[test]
        fn a_path_under_a_system_link_is_rewritten_through_it() {
            let fs = macos();
            // Nothing below the link is inspected, so none of it need exist.
            assert_eq!(
                fs.resolve("/var/folders/vt/T/ws/.ft/browser"),
                PathBuf::from("/private/var/folders/vt/T/ws/.ft/browser")
            );
            assert_eq!(
                fs.resolve("/tmp/ws/scrollback"),
                PathBuf::from("/private/tmp/ws/scrollback")
            );
            assert_eq!(fs.resolve("/var"), PathBuf::from("/private/var"));
        }

        #[test]
        fn a_path_without_system_links_is_borrowed_unchanged() {
            let fs = macos();
            for path in [
                "/private/var/folders/vt/T/ws",
                "/Users/me/ws",
                "/",
                "/absent/x",
            ] {
                let path = Path::new(path);
                let resolved = resolve_with(path, &|probe| fs.0.get(probe).cloned());
                assert!(matches!(resolved, Cow::Borrowed(_)), "{}", path.display());
                assert_eq!(resolved, path);
            }
        }

        /// ostree: `/home -> var/home`, and `/var` itself a link.
        #[test]
        fn links_to_links_and_absolute_targets_are_followed() {
            let fs = FakeFs::default()
                .dir("/", 0, 0o755)
                .link("/home", 0, "var/home")
                .link("/var", 0, "/sysroot/var")
                .dir("/sysroot", 0, 0o755);
            assert_eq!(
                fs.resolve("/home/me/.local/share/ft"),
                PathBuf::from("/sysroot/var/home/me/.local/share/ft")
            );
        }

        /// A cloned repository can carry a symlink; so can anything else the
        /// user unpacks.
        #[test]
        fn a_link_below_the_root_directory_is_left_for_the_walker_to_refuse() {
            // The user's own link, in the user's own directory.
            let own = macos().dir("/Users/me", USER, 0o755).link(
                "/Users/me/projects",
                USER,
                "/Volumes/big/projects",
            );
            assert_eq!(
                own.resolve("/Users/me/projects/ws"),
                PathBuf::from("/Users/me/projects/ws")
            );

            // Root's link in root's directory, under the user's tree: what a
            // container's bind mount or a `sudo` build leaves behind.
            let bind_mount = macos()
                .dir("/Users/me", USER, 0o755)
                .dir("/Users/me/repo", USER, 0o755)
                .dir("/Users/me/repo/.ft", 0, 0o755)
                .link("/Users/me/repo/.ft/browser_profiles", 0, "/Users/me/.ssh");
            assert_eq!(
                bind_mount.resolve("/Users/me/repo/.ft/browser_profiles/default"),
                PathBuf::from("/Users/me/repo/.ft/browser_profiles/default")
            );

            // Root all the way down: `sudo git clone` into `/opt`.
            let sudo_clone = FakeFs::default()
                .dir("/", 0, 0o755)
                .dir("/opt", 0, 0o755)
                .dir("/opt/repo", 0, 0o755)
                .link("/opt/repo/.ft", 0, "/root/.ssh");
            assert_eq!(
                sudo_clone.resolve("/opt/repo/.ft/browser"),
                PathBuf::from("/opt/repo/.ft/browser")
            );

            // Root's link in a sticky directory: anyone can hard-link one of
            // root's links in under a free name. Reached through `/tmp`, the
            // system link is followed and the planted one is not.
            let sticky = macos().dir("/private/tmp", 0, 0o1777).link(
                "/private/tmp/planted",
                0,
                "/private/etc",
            );
            assert_eq!(
                sticky.resolve("/tmp/planted/x"),
                PathBuf::from("/private/tmp/planted/x")
            );
        }

        #[test]
        fn a_link_in_the_root_directory_is_followed_only_when_root_alone_places_it() {
            let user_owned_link =
                FakeFs::default()
                    .dir("/", 0, 0o755)
                    .link("/data", USER, "/Users/me/data");
            assert_eq!(user_owned_link.resolve("/data/x"), PathBuf::from("/data/x"));

            for root_mode in [0o775, 0o757, 0o1777] {
                let writable_root = FakeFs::default()
                    .dir("/", 0, root_mode)
                    .link("/opt", 0, "usr/opt");
                assert_eq!(
                    writable_root.resolve("/opt/x"),
                    PathBuf::from("/opt/x"),
                    "{root_mode:o}"
                );
            }

            // A user namespace maps the real root to an unprivileged id.
            let unowned_root =
                FakeFs::default()
                    .dir("/", 65_534, 0o755)
                    .link("/var", 65_534, "private/var");
            assert_eq!(unowned_root.resolve("/var/x"), PathBuf::from("/var/x"));
        }

        #[test]
        fn relative_paths_parent_components_and_parent_targets_are_unchanged() {
            let fs = macos().link("/etc", 0, "../outside");
            assert_eq!(fs.resolve("var/folders"), PathBuf::from("var/folders"));
            assert_eq!(
                fs.resolve("/var/../etc/passwd"),
                PathBuf::from("/var/../etc/passwd")
            );
            assert_eq!(fs.resolve("/etc/hosts"), PathBuf::from("/etc/hosts"));
        }

        /// A chain longer than the limit, and a cycle, both end still
        /// naming a link, which the walker then refuses.
        #[test]
        fn the_hops_are_bounded() {
            let mut chain = FakeFs::default().dir("/", 0, 0o755);
            for hop in 0..MAX_HOPS + 5 {
                chain = chain.link(&format!("/l{hop}"), 0, &format!("l{}", hop + 1));
            }
            assert_eq!(
                chain.resolve("/l0/x"),
                PathBuf::from(format!("/l{MAX_HOPS}/x"))
            );

            let cycle = FakeFs::default()
                .dir("/", 0, 0o755)
                .link("/a", 0, "b")
                .link("/b", 0, "a");
            assert_eq!(cycle.resolve("/a/x"), PathBuf::from("/a/x"));
        }

        /// The probe itself, on the host's filesystem.
        #[test]
        fn inspect_tells_a_symlink_from_what_it_points_to() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target");
            let link = dir.path().join("link");
            std::fs::create_dir(&target).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let directory = inspect(&target).unwrap();
            assert!(directory.link_target.is_none());
            assert_eq!(directory.owner, rustix::process::geteuid().as_raw());

            let symlink = inspect(&link).unwrap();
            assert_eq!(symlink.link_target.as_deref(), Some(target.as_path()));
            assert_eq!(symlink.owner, rustix::process::geteuid().as_raw());

            assert!(inspect(&dir.path().join("absent")).is_none());
            assert!(inspect(Path::new("/")).unwrap().link_target.is_none());

            // The user's own link is not a system link.
            let through_link = link.join("x");
            assert_eq!(
                super::super::resolve_system_symlinks(&through_link)
                    .file_name()
                    .and_then(|name| name.to_str()),
                Some("x")
            );
            assert!(
                super::super::resolve_system_symlinks(&through_link)
                    .parent()
                    .is_some_and(|parent| parent.ends_with("link"))
            );
        }

        /// The real filesystem of the host: macOS's `/var` and `/tmp`.
        #[cfg(target_os = "macos")]
        #[test]
        fn macos_var_and_tmp_resolve_into_private() {
            assert_eq!(
                super::super::resolve_system_symlinks(Path::new("/var/folders")),
                Path::new("/private/var/folders")
            );
            assert_eq!(
                super::super::resolve_system_symlinks(Path::new("/tmp/ft-r937n-absent/leaf")),
                Path::new("/private/tmp/ft-r937n-absent/leaf")
            );
        }
    }
}
