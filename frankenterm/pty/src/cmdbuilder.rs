#[cfg(unix)]
use anyhow::Context;
#[cfg(feature = "serde_support")]
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(unix)]
use std::path::Component;
use std::path::Path;

/// Used to deal with Windows having case-insensitive environment variables.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde_support", derive(Serialize, Deserialize))]
struct EnvEntry {
    /// Whether or not this environment variable came from the base environment,
    /// as opposed to having been explicitly set by the caller.
    is_from_base_env: bool,

    /// For case-insensitive platforms, the environment variable key in its preferred casing.
    preferred_key: OsString,

    /// The environment variable value.
    value: OsString,
}

impl EnvEntry {
    fn map_key(k: OsString) -> OsString {
        if cfg!(windows) {
            // Best-effort lowercase transformation of an os string
            match k.to_str() {
                Some(s) => s.to_lowercase().into(),
                None => k,
            }
        } else {
            k
        }
    }
}

#[cfg(unix)]
fn get_shell() -> String {
    use nix::unistd::{access, AccessFlags};
    use std::ffi::CStr;
    use std::str;

    let ent = unsafe { libc::getpwuid(libc::getuid()) };
    if !ent.is_null() {
        let shell = unsafe { CStr::from_ptr((*ent).pw_shell) };
        match shell.to_str().map(str::to_owned) {
            Err(err) => {
                log::warn!(
                    "passwd database shell could not be \
                     represented as utf-8: {err:#}, \
                     falling back to /bin/sh"
                );
            }
            Ok(shell) => {
                if let Err(err) = access(Path::new(&shell), AccessFlags::X_OK) {
                    log::warn!(
                        "passwd database shell={shell:?} which is \
                         not executable ({err:#}), falling back to /bin/sh"
                    );
                } else {
                    return shell;
                }
            }
        }
    }
    "/bin/sh".into()
}

fn get_base_env() -> BTreeMap<OsString, EnvEntry> {
    let mut env: BTreeMap<OsString, EnvEntry> = std::env::vars_os()
        .map(|(key, value)| {
            (
                EnvEntry::map_key(key.clone()),
                EnvEntry {
                    is_from_base_env: true,
                    preferred_key: key,
                    value,
                },
            )
        })
        .collect();

    #[cfg(unix)]
    {
        let key = EnvEntry::map_key("SHELL".into());
        // Only set the value of SHELL if it isn't already set
        if !env.contains_key(&key) {
            env.insert(
                EnvEntry::map_key("SHELL".into()),
                EnvEntry {
                    is_from_base_env: true,
                    preferred_key: "SHELL".into(),
                    value: get_shell().into(),
                },
            );
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use winapi::um::processenv::ExpandEnvironmentStringsW;
        use winreg::enums::{RegType, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        use winreg::types::FromRegValue;
        use winreg::{RegKey, RegValue};

        fn reg_value_to_string(value: &RegValue) -> anyhow::Result<OsString> {
            match value.vtype {
                RegType::REG_EXPAND_SZ => {
                    let src = unsafe {
                        std::slice::from_raw_parts(
                            value.bytes.as_ptr() as *const u16,
                            value.bytes.len() / 2,
                        )
                    };
                    let size =
                        unsafe { ExpandEnvironmentStringsW(src.as_ptr(), std::ptr::null_mut(), 0) };
                    let mut buf = vec![0u16; size as usize + 1];
                    unsafe {
                        ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
                    };

                    let mut buf = buf.as_slice();
                    while let Some(0) = buf.last() {
                        buf = &buf[0..buf.len() - 1];
                    }
                    Ok(OsString::from_wide(buf))
                }
                _ => Ok(OsString::from_reg_value(value)?),
            }
        }

        if let Ok(sys_env) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey("System\\CurrentControlSet\\Control\\Session Manager\\Environment")
        {
            for res in sys_env.enum_values() {
                if let Ok((name, value)) = res {
                    if name.to_ascii_lowercase() == "username" {
                        continue;
                    }
                    if let Ok(value) = reg_value_to_string(&value) {
                        log::trace!("adding SYS env: {:?} {:?}", name, value);
                        env.insert(
                            EnvEntry::map_key(name.clone().into()),
                            EnvEntry {
                                is_from_base_env: true,
                                preferred_key: name.into(),
                                value,
                            },
                        );
                    }
                }
            }
        }

        if let Ok(sys_env) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment") {
            for res in sys_env.enum_values() {
                if let Ok((name, value)) = res {
                    if let Ok(value) = reg_value_to_string(&value) {
                        // Merge the system and user paths together
                        let value = if name.to_ascii_lowercase() == "path" {
                            match env.get(&EnvEntry::map_key(name.clone().into())) {
                                Some(entry) => {
                                    let mut result = OsString::new();
                                    result.push(&entry.value);
                                    result.push(";");
                                    result.push(&value);
                                    result
                                }
                                None => value,
                            }
                        } else {
                            value
                        };

                        log::trace!("adding USER env: {:?} {:?}", name, value);
                        env.insert(
                            EnvEntry::map_key(name.clone().into()),
                            EnvEntry {
                                is_from_base_env: true,
                                preferred_key: name.into(),
                                value,
                            },
                        );
                    }
                }
            }
        }
    }

    env
}

/// `CommandBuilder` is used to prepare a command to be spawned into a pty.
/// The interface is intentionally similar to that of `std::process::Command`.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde_support", derive(Serialize, Deserialize))]
pub struct CommandBuilder {
    args: Vec<OsString>,
    envs: BTreeMap<OsString, EnvEntry>,
    cwd: Option<OsString>,
    #[cfg(unix)]
    pub(crate) umask: Option<libc::mode_t>,
    controlling_tty: bool,
}

impl CommandBuilder {
    /// Create a new builder instance with argv\[0\] set to the specified
    /// program.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            args: vec![program.as_ref().to_owned()],
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
            controlling_tty: true,
        }
    }

    /// Create a new builder instance from a pre-built argument vector
    pub fn from_argv(args: Vec<OsString>) -> Self {
        Self {
            args,
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
            controlling_tty: true,
        }
    }

    /// Set whether we should set the pty as the controlling terminal.
    /// The default is true, which is usually what you want, but you
    /// may need to set this to false if you are crossing container
    /// boundaries (eg: flatpak) to workaround issues like:
    /// <https://github.com/flatpak/flatpak/issues/3697>
    /// <https://github.com/flatpak/flatpak/issues/3285>
    pub fn set_controlling_tty(&mut self, controlling_tty: bool) {
        self.controlling_tty = controlling_tty;
    }

    pub fn get_controlling_tty(&self) -> bool {
        self.controlling_tty
    }

    /// Create a new builder instance that will run some idea of a default
    /// program.  Such a builder will panic if `arg` is called on it.
    pub fn new_default_prog() -> Self {
        Self {
            args: vec![],
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
            controlling_tty: true,
        }
    }

    /// Returns true if this builder was created via `new_default_prog`
    pub fn is_default_prog(&self) -> bool {
        self.args.is_empty()
    }

    /// Append an argument to the current command line.
    /// Will panic if called on a builder created via `new_default_prog`.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) {
        if self.is_default_prog() {
            panic!("attempted to add args to a default_prog builder");
        }
        self.args.push(arg.as_ref().to_owned());
    }

    /// If a builder is_default_prog, then this function can be used to
    /// set the actual prog that should be used.
    /// This is intended to facilitate plumbing through the handling
    /// of the underlying default prog when merging together supplemental
    /// env and cwd information.
    /// You will not typically use this method in your own code.
    pub fn replace_default_prog(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
        if !self.is_default_prog() {
            panic!("attempted to replace_default_prog on a non-default_prog builder");
        }

        for arg in args {
            self.args.push(arg.as_ref().to_owned());
        }
    }

    /// Append a sequence of arguments to the current command line
    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
    }

    pub fn get_argv(&self) -> &Vec<OsString> {
        &self.args
    }

    pub fn get_argv_mut(&mut self) -> &mut Vec<OsString> {
        &mut self.args
    }

    /// Override the value of an environmental variable
    pub fn env<K, V>(&mut self, key: K, value: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let key: OsString = key.as_ref().into();
        let value: OsString = value.as_ref().into();
        self.envs.insert(
            EnvEntry::map_key(key.clone()),
            EnvEntry {
                is_from_base_env: false,
                preferred_key: key,
                value: value,
            },
        );
    }

    pub fn env_remove<K>(&mut self, key: K)
    where
        K: AsRef<OsStr>,
    {
        let key = key.as_ref().into();
        self.envs.remove(&EnvEntry::map_key(key));
    }

    pub fn env_clear(&mut self) {
        self.envs.clear();
    }

    pub fn get_env<K>(&self, key: K) -> Option<&OsStr>
    where
        K: AsRef<OsStr>,
    {
        let key = key.as_ref().into();
        self.envs.get(&EnvEntry::map_key(key)).map(
            |EnvEntry {
                 is_from_base_env: _,
                 preferred_key: _,
                 value,
             }| value.as_os_str(),
        )
    }

    pub fn cwd<D>(&mut self, dir: D)
    where
        D: AsRef<OsStr>,
    {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    pub fn clear_cwd(&mut self) {
        self.cwd.take();
    }

    pub fn get_cwd(&self) -> Option<&OsString> {
        self.cwd.as_ref()
    }

    /// Iterate over the configured environment. Only includes environment
    /// variables set by the caller via `env`, not variables set in the base
    /// environment.
    pub fn iter_extra_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.envs.values().filter_map(
            |EnvEntry {
                 is_from_base_env,
                 preferred_key,
                 value,
             }| {
                if *is_from_base_env {
                    None
                } else {
                    let key = preferred_key.to_str()?;
                    let value = value.to_str()?;
                    Some((key, value))
                }
            },
        )
    }

    pub fn iter_full_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.envs.values().filter_map(
            |EnvEntry {
                 preferred_key,
                 value,
                 ..
             }| {
                let key = preferred_key.to_str()?;
                let value = value.to_str()?;
                Some((key, value))
            },
        )
    }

    /// Return the configured command and arguments as a single string,
    /// quoted per the unix shell conventions.
    pub fn as_unix_command_line(&self) -> anyhow::Result<String> {
        let mut strs = vec![];
        for arg in &self.args {
            let s = arg
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("argument cannot be represented as utf8"))?;
            strs.push(s);
        }
        Ok(shell_words::join(strs))
    }
}

#[cfg(unix)]
impl CommandBuilder {
    pub fn umask(&mut self, mask: Option<libc::mode_t>) {
        self.umask = mask;
    }

    fn resolve_path(&self) -> Option<&OsStr> {
        self.get_env("PATH")
    }

    fn search_path(&self, exe: &OsStr, cwd: &OsStr) -> anyhow::Result<OsString> {
        use nix::unistd::{access, AccessFlags};

        let exe_path: &Path = exe.as_ref();
        if exe_path.is_relative() {
            let cwd: &Path = cwd.as_ref();
            let mut errors = vec![];

            // If the requested executable is explicitly relative to cwd,
            // then check only there.
            if is_cwd_relative_path(exe_path) {
                let abs_path = cwd.join(exe_path);

                if abs_path.is_dir() {
                    anyhow::bail!(
                        "Unable to spawn {} because it is a directory",
                        abs_path.display()
                    );
                } else if access(&abs_path, AccessFlags::X_OK).is_ok() {
                    return Ok(abs_path.into_os_string());
                } else if access(&abs_path, AccessFlags::F_OK).is_ok() {
                    anyhow::bail!(
                        "Unable to spawn {} because it is not executable",
                        abs_path.display()
                    );
                }

                anyhow::bail!(
                    "Unable to spawn {} because it does not exist",
                    abs_path.display()
                );
            }

            if let Some(path) = self.resolve_path() {
                for path in std::env::split_paths(&path) {
                    let candidate = cwd.join(&path).join(&exe);

                    if candidate.is_dir() {
                        errors.push(format!("{} exists but is a directory", candidate.display()));
                    } else if access(&candidate, AccessFlags::X_OK).is_ok() {
                        return Ok(candidate.into_os_string());
                    } else if access(&candidate, AccessFlags::F_OK).is_ok() {
                        errors.push(format!(
                            "{} exists but is not executable",
                            candidate.display()
                        ));
                    }
                }
                errors.push(format!("No viable candidates found in PATH {path:?}"));
            } else {
                errors.push("Unable to resolve the PATH".to_string());
            }
            anyhow::bail!(
                "Unable to spawn {} because:\n{}",
                exe_path.display(),
                errors.join(".\n")
            );
        } else if exe_path.is_dir() {
            anyhow::bail!(
                "Unable to spawn {} because it is a directory",
                exe_path.display()
            );
        } else {
            if let Err(err) = access(exe_path, AccessFlags::X_OK) {
                if access(exe_path, AccessFlags::F_OK).is_ok() {
                    anyhow::bail!(
                        "Unable to spawn {} because it is not executable ({err:#})",
                        exe_path.display()
                    );
                } else {
                    anyhow::bail!(
                        "Unable to spawn {} because it doesn't exist on the filesystem ({err:#})",
                        exe_path.display()
                    );
                }
            }

            Ok(exe.to_owned())
        }
    }

    /// Convert the CommandBuilder to a `std::process::Command` instance.
    pub(crate) fn as_command(&self) -> anyhow::Result<std::process::Command> {
        use std::os::unix::process::CommandExt;

        let home = self.get_home_dir()?;
        let dir: &OsStr = self
            .cwd
            .as_ref()
            .map(|dir| dir.as_os_str())
            .filter(|dir| std::path::Path::new(dir).is_dir())
            .unwrap_or(home.as_ref());
        let shell = self.get_shell();

        let mut cmd = if self.is_default_prog() {
            let mut cmd = std::process::Command::new(&shell);

            // Run the shell as a login shell by prefixing the shell's
            // basename with `-` and setting that as argv0
            let basename = shell.rsplit('/').next().unwrap_or(&shell);
            cmd.arg0(&format!("-{}", basename));
            cmd
        } else {
            let resolved = self.search_path(&self.args[0], dir)?;
            let mut cmd = std::process::Command::new(&resolved);
            cmd.arg0(&self.args[0]);
            cmd.args(&self.args[1..]);
            cmd
        };

        cmd.current_dir(dir);

        cmd.env_clear();
        cmd.env("SHELL", shell);
        cmd.envs(self.envs.values().map(
            |EnvEntry {
                 is_from_base_env: _,
                 preferred_key,
                 value,
             }| (preferred_key.as_os_str(), value.as_os_str()),
        ));

        Ok(cmd)
    }

    /// Determine which shell to run.
    /// We take the contents of the $SHELL env var first, then
    /// fall back to looking it up from the password database.
    pub fn get_shell(&self) -> String {
        use nix::unistd::{access, AccessFlags};

        if let Some(shell) = self.get_env("SHELL").and_then(OsStr::to_str) {
            match access(shell, AccessFlags::X_OK) {
                Ok(()) => return shell.into(),
                Err(err) => log::warn!(
                    "$SHELL -> {shell:?} which is \
                     not executable ({err:#}), falling back to password db lookup"
                ),
            }
        }

        get_shell().into()
    }

    fn get_home_dir(&self) -> anyhow::Result<String> {
        if let Some(home_dir) = self.get_env("HOME").and_then(OsStr::to_str) {
            return Ok(home_dir.into());
        }

        let ent = unsafe { libc::getpwuid(libc::getuid()) };
        if ent.is_null() {
            Ok("/".into())
        } else {
            use std::ffi::CStr;
            use std::str;
            let home = unsafe { CStr::from_ptr((*ent).pw_dir) };
            home.to_str()
                .map(str::to_owned)
                .context("failed to resolve home dir")
        }
    }
}

#[cfg(windows)]
impl CommandBuilder {
    fn search_path(&self, exe: &OsStr) -> OsString {
        if let Some(path) = self.get_env("PATH") {
            let extensions = self.get_env("PATHEXT").unwrap_or(OsStr::new(".EXE"));
            for path in std::env::split_paths(&path) {
                // Check for exactly the user's string in this path dir
                let candidate = path.join(&exe);
                if candidate.exists() {
                    return candidate.into_os_string();
                }

                // otherwise try tacking on some extensions.
                // Note that this really replaces the extension in the
                // user specified path, so this is potentially wrong.
                for ext in std::env::split_paths(&extensions) {
                    // PATHEXT includes the leading `.`, but `with_extension`
                    // doesn't want that
                    let ext = ext.to_str().expect("PATHEXT entries must be utf8");
                    let path = path.join(&exe).with_extension(&ext[1..]);
                    if path.exists() {
                        return path.into_os_string();
                    }
                }
            }
        }

        exe.to_owned()
    }

    pub(crate) fn current_directory(&self) -> Option<Vec<u16>> {
        let home: Option<&OsStr> = self
            .get_env("USERPROFILE")
            .filter(|path| Path::new(path).is_dir());
        let cwd: Option<&OsStr> = self.cwd.as_deref().filter(|path| Path::new(path).is_dir());
        let dir: Option<&OsStr> = cwd.or(home);

        dir.map(|dir| {
            let mut wide = vec![];

            if Path::new(dir).is_relative() {
                if let Ok(ccwd) = std::env::current_dir() {
                    wide.extend(ccwd.join(dir).as_os_str().encode_wide());
                } else {
                    wide.extend(dir.encode_wide());
                }
            } else {
                wide.extend(dir.encode_wide());
            }

            wide.push(0);
            wide
        })
    }

    /// Constructs an environment block for this spawn attempt.
    /// Uses the current process environment as the base and then
    /// adds/replaces the environment that was specified via the
    /// `env` methods.
    pub(crate) fn environment_block(&self) -> Vec<u16> {
        // encode the environment as wide characters
        let mut block = vec![];

        for EnvEntry {
            is_from_base_env: _,
            preferred_key,
            value,
        } in self.envs.values()
        {
            block.extend(preferred_key.encode_wide());
            block.push(b'=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        // and a final terminator for CreateProcessW
        block.push(0);

        block
    }

    pub fn get_shell(&self) -> String {
        let exe: OsString = self
            .get_env("ComSpec")
            .unwrap_or(OsStr::new("cmd.exe"))
            .into();
        exe.into_string()
            .unwrap_or_else(|_| "%CompSpec%".to_string())
    }

    pub(crate) fn cmdline(&self) -> anyhow::Result<(Vec<u16>, Vec<u16>)> {
        let mut cmdline = Vec::<u16>::new();

        let exe: OsString = if self.is_default_prog() {
            self.get_env("ComSpec")
                .unwrap_or(OsStr::new("cmd.exe"))
                .into()
        } else {
            self.search_path(&self.args[0])
        };

        Self::append_quoted(&exe, &mut cmdline);

        // Ensure that we nul terminate the module name, otherwise we'll
        // ask CreateProcessW to start something random!
        let mut exe: Vec<u16> = exe.encode_wide().collect();
        exe.push(0);

        for arg in self.args.iter().skip(1) {
            cmdline.push(' ' as u16);
            anyhow::ensure!(
                !arg.encode_wide().any(|c| c == 0),
                "invalid encoding for command line argument {:?}",
                arg
            );
            Self::append_quoted(arg, &mut cmdline);
        }
        // Ensure that the command line is nul terminated too!
        cmdline.push(0);
        Ok((exe, cmdline))
    }

    // Borrowed from https://github.com/hniksic/rust-subprocess/blob/873dfed165173e52907beb87118b2c0c05d8b8a1/src/popen.rs#L1117
    // which in turn was translated from ArgvQuote at http://tinyurl.com/zmgtnls
    fn append_quoted(arg: &OsStr, cmdline: &mut Vec<u16>) {
        if !arg.is_empty()
            && !arg.encode_wide().any(|c| {
                c == ' ' as u16
                    || c == '\t' as u16
                    || c == '\n' as u16
                    || c == '\x0b' as u16
                    || c == '\"' as u16
            })
        {
            cmdline.extend(arg.encode_wide());
            return;
        }
        cmdline.push('"' as u16);

        let arg: Vec<_> = arg.encode_wide().collect();
        let mut i = 0;
        while i < arg.len() {
            let mut num_backslashes = 0;
            while i < arg.len() && arg[i] == '\\' as u16 {
                i += 1;
                num_backslashes += 1;
            }

            if i == arg.len() {
                for _ in 0..num_backslashes * 2 {
                    cmdline.push('\\' as u16);
                }
                break;
            } else if arg[i] == b'"' as u16 {
                for _ in 0..num_backslashes * 2 + 1 {
                    cmdline.push('\\' as u16);
                }
                cmdline.push(arg[i]);
            } else {
                for _ in 0..num_backslashes {
                    cmdline.push('\\' as u16);
                }
                cmdline.push(arg[i]);
            }
            i += 1;
        }
        cmdline.push('"' as u16);
    }
}

#[cfg(unix)]
/// Returns true if the path begins with `./` or `../`
fn is_cwd_relative_path<P: AsRef<Path>>(p: P) -> bool {
    matches!(
        p.as_ref().components().next(),
        Some(Component::CurDir | Component::ParentDir)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_cwd_relative() {
        assert!(is_cwd_relative_path("."));
        assert!(is_cwd_relative_path("./foo"));
        assert!(is_cwd_relative_path("../foo"));
        assert!(!is_cwd_relative_path("foo"));
        assert!(!is_cwd_relative_path("/foo"));
    }

    #[test]
    fn test_env() {
        let mut cmd = CommandBuilder::new("dummy");
        let package_authors = cmd.get_env("CARGO_PKG_AUTHORS");
        println!("package_authors: {:?}", package_authors);
        assert!(package_authors == Some(OsStr::new("Wez Furlong")));

        cmd.env("foo key", "foo value");
        cmd.env("bar key", "bar value");

        let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
        println!("iterated_envs: {:?}", iterated_envs);
        assert!(iterated_envs == vec![("bar key", "bar value"), ("foo key", "foo value")]);

        {
            let mut cmd = cmd.clone();
            cmd.env_remove("foo key");

            let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
            println!("iterated_envs: {:?}", iterated_envs);
            assert!(iterated_envs == vec![("bar key", "bar value")]);
        }

        {
            let mut cmd = cmd.clone();
            cmd.env_remove("bar key");

            let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
            println!("iterated_envs: {:?}", iterated_envs);
            assert!(iterated_envs == vec![("foo key", "foo value")]);
        }

        {
            let mut cmd = cmd.clone();
            cmd.env_clear();

            let iterated_envs = cmd.iter_extra_env_as_str().collect::<Vec<_>>();
            println!("iterated_envs: {:?}", iterated_envs);
            assert!(iterated_envs.is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_env_case_insensitive_override() {
        let mut cmd = CommandBuilder::new("dummy");
        cmd.env("Cargo_Pkg_Authors", "Not Wez");
        assert!(cmd.get_env("cargo_pkg_authors") == Some(OsStr::new("Not Wez")));

        cmd.env_remove("cARGO_pKG_aUTHORS");
        assert!(cmd.get_env("CARGO_PKG_AUTHORS").is_none());
    }

    // ── CommandBuilder: construction ────────────────────────

    #[test]
    fn new_sets_program_as_argv0() {
        let cmd = CommandBuilder::new("myapp");
        assert_eq!(cmd.get_argv().len(), 1);
        assert_eq!(cmd.get_argv()[0], OsStr::new("myapp"));
    }

    #[test]
    fn from_argv_preserves_args() {
        let args = vec![
            OsString::from("myapp"),
            OsString::from("--flag"),
            OsString::from("value"),
        ];
        let cmd = CommandBuilder::from_argv(args.clone());
        assert_eq!(*cmd.get_argv(), args);
    }

    #[test]
    fn new_default_prog_is_empty() {
        let cmd = CommandBuilder::new_default_prog();
        assert!(cmd.is_default_prog());
        assert!(cmd.get_argv().is_empty());
    }

    #[test]
    fn new_is_not_default_prog() {
        let cmd = CommandBuilder::new("bash");
        assert!(!cmd.is_default_prog());
    }

    // ── CommandBuilder: arg / args ──────────────────────────

    #[test]
    fn arg_appends() {
        let mut cmd = CommandBuilder::new("git");
        cmd.arg("status");
        cmd.arg("--short");
        assert_eq!(cmd.get_argv().len(), 3);
        assert_eq!(cmd.get_argv()[1], OsStr::new("status"));
        assert_eq!(cmd.get_argv()[2], OsStr::new("--short"));
    }

    #[test]
    fn args_appends_multiple() {
        let mut cmd = CommandBuilder::new("ls");
        cmd.args(&["-l", "-a", "/tmp"]);
        assert_eq!(cmd.get_argv().len(), 4);
    }

    #[test]
    #[should_panic(expected = "attempted to add args to a default_prog builder")]
    fn arg_panics_on_default_prog() {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.arg("boom");
    }

    // ── CommandBuilder: replace_default_prog ─────────────────

    #[test]
    fn replace_default_prog_works() {
        let mut cmd = CommandBuilder::new_default_prog();
        assert!(cmd.is_default_prog());
        cmd.replace_default_prog(["zsh", "-l"]);
        assert!(!cmd.is_default_prog());
        assert_eq!(cmd.get_argv()[0], OsStr::new("zsh"));
        assert_eq!(cmd.get_argv()[1], OsStr::new("-l"));
    }

    #[test]
    #[should_panic(expected = "attempted to replace_default_prog on a non-default_prog builder")]
    fn replace_default_prog_panics_on_non_default() {
        let mut cmd = CommandBuilder::new("bash");
        cmd.replace_default_prog(["zsh"]);
    }

    // ── CommandBuilder: controlling_tty ──────────────────────

    #[test]
    fn controlling_tty_default_true() {
        let cmd = CommandBuilder::new("bash");
        assert!(cmd.get_controlling_tty());
    }

    #[test]
    fn controlling_tty_set_false() {
        let mut cmd = CommandBuilder::new("bash");
        cmd.set_controlling_tty(false);
        assert!(!cmd.get_controlling_tty());
    }

    // ── CommandBuilder: cwd ─────────────────────────────────

    #[test]
    fn cwd_none_by_default() {
        let cmd = CommandBuilder::new("bash");
        assert!(cmd.get_cwd().is_none());
    }

    #[test]
    fn cwd_set_and_get() {
        let mut cmd = CommandBuilder::new("bash");
        cmd.cwd("/tmp");
        assert_eq!(cmd.get_cwd(), Some(&OsString::from("/tmp")));
    }

    #[test]
    fn cwd_clear() {
        let mut cmd = CommandBuilder::new("bash");
        cmd.cwd("/tmp");
        cmd.clear_cwd();
        assert!(cmd.get_cwd().is_none());
    }

    // ── CommandBuilder: get_argv_mut ─────────────────────────

    #[test]
    fn get_argv_mut_allows_modification() {
        let mut cmd = CommandBuilder::new("bash");
        cmd.get_argv_mut().push(OsString::from("-c"));
        assert_eq!(cmd.get_argv().len(), 2);
        assert_eq!(cmd.get_argv()[1], OsStr::new("-c"));
    }

    // ── CommandBuilder: as_unix_command_line ──────────────────

    #[test]
    fn as_unix_command_line_simple() {
        let cmd = CommandBuilder::new("echo");
        let cl = cmd.as_unix_command_line().unwrap();
        assert_eq!(cl, "echo");
    }

    #[test]
    fn as_unix_command_line_with_args() {
        let mut cmd = CommandBuilder::new("ls");
        cmd.arg("-la");
        cmd.arg("/tmp");
        let cl = cmd.as_unix_command_line().unwrap();
        assert_eq!(cl, "ls -la /tmp");
    }

    #[test]
    fn as_unix_command_line_quotes_spaces() {
        let mut cmd = CommandBuilder::new("echo");
        cmd.arg("hello world");
        let cl = cmd.as_unix_command_line().unwrap();
        assert!(cl.contains("'hello world'") || cl.contains("\"hello world\""));
    }

    // ── CommandBuilder: Clone / Debug / PartialEq ────────────

    #[test]
    fn command_builder_clone_eq() {
        let mut cmd = CommandBuilder::new("test");
        cmd.arg("--flag");
        cmd.env("MY_VAR", "my_value");
        let cloned = cmd.clone();
        assert_eq!(cmd, cloned);
    }

    #[test]
    fn command_builder_debug() {
        let cmd = CommandBuilder::new("test");
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("CommandBuilder"));
        assert!(dbg.contains("test"));
    }

    // ── CommandBuilder: iter_full_env_as_str ──────────────────

    #[test]
    fn iter_full_env_includes_base_and_extra() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("CUSTOM_KEY", "custom_value");
        let full: Vec<_> = cmd.iter_full_env_as_str().collect();
        // Should include base env vars (like PATH) plus our custom one
        assert!(full
            .iter()
            .any(|(k, v)| *k == "CUSTOM_KEY" && *v == "custom_value"));
        // Should have more than just our one custom var
        assert!(full.len() > 1);
    }

    // ── EnvEntry::map_key ────────────────────────────────────

    #[test]
    fn env_entry_map_key_unix_identity() {
        // On unix, map_key is identity
        let key = OsString::from("MyKey");
        let mapped = EnvEntry::map_key(key.clone());
        if cfg!(unix) {
            assert_eq!(mapped, key);
        }
    }

    // ── CommandBuilder: get_shell ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn get_shell_returns_nonempty() {
        let cmd = CommandBuilder::new("test");
        let shell = cmd.get_shell();
        assert!(!shell.is_empty());
        // Should be an absolute path on unix
        assert!(shell.starts_with('/'));
    }

    // ── Second-pass expansion ────────────────────────────────────

    #[test]
    fn env_set_and_get() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("MY_KEY", "my_value");
        assert_eq!(cmd.get_env("MY_KEY"), Some(OsStr::new("my_value")));
    }

    #[test]
    fn env_override_value() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("KEY", "first");
        cmd.env("KEY", "second");
        assert_eq!(cmd.get_env("KEY"), Some(OsStr::new("second")));
    }

    #[test]
    fn env_remove_nonexistent_is_noop() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env_remove("DOES_NOT_EXIST_XYZ_UNIQUE");
        // Should not panic
    }

    #[test]
    fn env_clear_removes_all() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("A", "1");
        cmd.env("B", "2");
        cmd.env_clear();
        assert!(cmd.get_env("A").is_none());
        assert!(cmd.get_env("B").is_none());
    }

    #[test]
    fn env_clear_then_set() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env_clear();
        cmd.env("FRESH", "value");
        assert_eq!(cmd.get_env("FRESH"), Some(OsStr::new("value")));
    }

    #[test]
    fn get_env_nonexistent_returns_none() {
        let cmd = CommandBuilder::new("test");
        assert!(cmd.get_env("ABSOLUTELY_NONEXISTENT_VAR_XYZ").is_none());
    }

    #[test]
    fn iter_extra_env_empty_when_no_extras() {
        let cmd = CommandBuilder::new("test");
        let extras: Vec<_> = cmd.iter_extra_env_as_str().collect();
        assert!(extras.is_empty());
    }

    #[test]
    fn iter_extra_env_shows_only_extras() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("EXTRA_ONE", "val1");
        let extras: Vec<_> = cmd.iter_extra_env_as_str().collect();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], ("EXTRA_ONE", "val1"));
    }

    #[test]
    fn from_argv_empty_is_default_prog() {
        let cmd = CommandBuilder::from_argv(vec![]);
        assert!(cmd.is_default_prog());
    }

    #[test]
    fn from_argv_single_element() {
        let cmd = CommandBuilder::from_argv(vec![OsString::from("zsh")]);
        assert!(!cmd.is_default_prog());
        assert_eq!(cmd.get_argv()[0], OsStr::new("zsh"));
    }

    #[test]
    fn as_unix_command_line_default_prog() {
        let cmd = CommandBuilder::new_default_prog();
        let cl = cmd.as_unix_command_line().unwrap();
        assert_eq!(cl, "");
    }

    #[test]
    fn as_unix_command_line_with_special_chars() {
        let mut cmd = CommandBuilder::new("echo");
        cmd.arg("hello;world");
        let cl = cmd.as_unix_command_line().unwrap();
        // shell_words should quote this
        assert!(cl.contains("hello;world"));
    }

    #[test]
    fn command_builder_ne_different_args() {
        let a = CommandBuilder::new("foo");
        let mut b = CommandBuilder::new("foo");
        b.arg("extra");
        assert_ne!(a, b);
    }

    #[test]
    fn command_builder_ne_different_program() {
        let a = CommandBuilder::new("foo");
        let b = CommandBuilder::new("bar");
        assert_ne!(a, b);
    }

    #[test]
    fn cwd_set_multiple_times_last_wins() {
        let mut cmd = CommandBuilder::new("test");
        cmd.cwd("/first");
        cmd.cwd("/second");
        assert_eq!(cmd.get_cwd(), Some(&OsString::from("/second")));
    }

    #[test]
    fn controlling_tty_toggle() {
        let mut cmd = CommandBuilder::new("test");
        assert!(cmd.get_controlling_tty());
        cmd.set_controlling_tty(false);
        assert!(!cmd.get_controlling_tty());
        cmd.set_controlling_tty(true);
        assert!(cmd.get_controlling_tty());
    }

    #[test]
    fn get_argv_mut_clear() {
        let mut cmd = CommandBuilder::new("test");
        cmd.arg("--flag");
        cmd.get_argv_mut().clear();
        assert!(cmd.is_default_prog());
    }

    #[test]
    fn clone_is_independent() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("KEY", "value");
        let mut cloned = cmd.clone();
        cloned.env("KEY", "different");
        assert_eq!(cmd.get_env("KEY"), Some(OsStr::new("value")));
        assert_eq!(cloned.get_env("KEY"), Some(OsStr::new("different")));
    }

    #[test]
    fn env_with_empty_value() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("EMPTY", "");
        assert_eq!(cmd.get_env("EMPTY"), Some(OsStr::new("")));
    }

    #[test]
    fn env_with_special_chars_in_value() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("SPECIAL", "foo=bar;baz&qux");
        assert_eq!(
            cmd.get_env("SPECIAL"),
            Some(OsStr::new("foo=bar;baz&qux"))
        );
    }

    // ── Third-pass expansion ────────────────────────────────────

    #[test]
    fn from_argv_preserves_order() {
        let args = vec![
            OsString::from("prog"),
            OsString::from("arg1"),
            OsString::from("arg2"),
        ];
        let cmd = CommandBuilder::from_argv(args.clone());
        assert_eq!(*cmd.get_argv(), args);
    }

    #[test]
    fn from_argv_empty_vec_is_default() {
        let cmd = CommandBuilder::from_argv(vec![]);
        assert!(cmd.is_default_prog());
    }

    #[test]
    fn env_remove_missing_key_is_noop() {
        let mut cmd = CommandBuilder::new("test");
        // Should not panic
        cmd.env_remove("NONEXISTENT_VAR_12345");
    }

    #[test]
    fn env_clear_drops_all_vars() {
        let mut cmd = CommandBuilder::new("test");
        cmd.env("A", "1");
        cmd.env("B", "2");
        cmd.env_clear();
        assert_eq!(cmd.get_env("A"), None);
        assert_eq!(cmd.get_env("B"), None);
    }

    #[test]
    fn clear_cwd_after_set() {
        let mut cmd = CommandBuilder::new("test");
        cmd.cwd("/some/path");
        assert!(cmd.get_cwd().is_some());
        cmd.clear_cwd();
        assert!(cmd.get_cwd().is_none());
    }

    #[test]
    fn new_default_prog_flag() {
        let cmd = CommandBuilder::new_default_prog();
        assert!(cmd.is_default_prog());
    }

    #[test]
    fn new_with_prog_not_default() {
        let cmd = CommandBuilder::new("test");
        assert!(!cmd.is_default_prog());
    }

    #[test]
    fn args_adds_multiple() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.args(["a", "b", "c"]);
        assert_eq!(cmd.get_argv().len(), 4); // prog + a + b + c
    }

    #[test]
    fn controlling_tty_is_true_by_default() {
        let cmd = CommandBuilder::new("test");
        assert!(cmd.get_controlling_tty());
    }
}
