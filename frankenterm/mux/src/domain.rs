//! A Domain represents an instance of a multiplexer.
//! For example, the gui frontend has its own domain,
//! and we can connect to a domain hosted by a mux server
//! that may be local, running "remotely" inside a WSL
//! container or actually remote, running on the other end
//! of an ssh session somewhere.

use crate::client::ClientId;
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, Pane, PaneId};
use crate::tab::{SplitRequest, Tab};
use crate::window::WindowId;
use crate::{
    MoveCommitReceipt, Mux, PaneOperationGuard, PaneRegistrationHandle, SplitCommitReceipt,
};
use anyhow::{bail, Context, Error};
use async_trait::async_trait;
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::{configuration, ExecDomain, SerialDomain, ValueOrFunc, WslDomain};
use downcast_rs::{impl_downcast, Downcast};
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_term::TerminalSize;
use parking_lot::Mutex;
use portable_pty::{
    native_pty_system, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, PtySystem,
};
use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

static DOMAIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type DomainId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainState {
    Detached,
    Attached,
}

pub fn alloc_domain_id() -> DomainId {
    crate::next_unique_usize_id(&DOMAIN_ID, "mux domain")
}

pub(crate) fn register_spawned_pane_or_rollback(
    mux: &Arc<Mux>,
    pane: &Arc<dyn Pane>,
) -> anyhow::Result<()> {
    if let Err(error) = mux.add_pane(pane) {
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| pane.kill()),
        );
        if rollback.is_err() {
            log::error!(
                "spawned-pane registration rollback panicked for exact pane identity {:p}",
                Arc::as_ptr(pane)
            );
        }
        return Err(error);
    }
    Ok(())
}

/// An exact pane process/PTY that has not yet been published in a mux.
///
/// Construction is crate-private so implementations outside this crate cannot
/// claim the unpublished-pane invariant without going through a mux-owned
/// domain implementation. Until `UnpublishedPane::into_pane` consumes the
/// reservation, dropping it kills the pane exactly once. This makes async
/// cancellation and every fallible pre-publication step fail closed without
/// briefly exposing an orphan pane registration.
#[must_use = "an unpublished pane must be published or allowed to roll back"]
pub struct UnpublishedPane {
    pane: Option<Arc<dyn Pane>>,
}

impl UnpublishedPane {
    pub(crate) fn new(pane: Arc<dyn Pane>) -> Self {
        Self { pane: Some(pane) }
    }

    pub(crate) fn pane(&self) -> &Arc<dyn Pane> {
        self.pane
            .as_ref()
            .expect("unpublished pane accessed after it was consumed")
    }

    pub(crate) fn into_pane(mut self) -> Arc<dyn Pane> {
        self.pane
            .take()
            .expect("unpublished pane consumed more than once")
    }
}

impl Drop for UnpublishedPane {
    fn drop(&mut self) {
        let Some(pane) = self.pane.take() else {
            return;
        };
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| pane.kill()),
        );
        if rollback.is_err() {
            log::error!(
                "unpublished-pane rollback panicked for exact pane identity {:p}",
                Arc::as_ptr(&pane)
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SplitSource {
    Spawn {
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    },
    MovePane(PaneId),
}

pub(super) struct PreparedPane {
    pane: Arc<dyn Pane>,
    registration: PaneRegistrationHandle,
    armed: bool,
}

impl PreparedPane {
    pub(super) fn new(pane: Arc<dyn Pane>, registration: PaneRegistrationHandle) -> Self {
        Self {
            pane,
            registration,
            armed: true,
        }
    }

    fn commit_split(
        mut self,
        tab: Arc<Tab>,
        window_id: WindowId,
        size: TerminalSize,
    ) -> SplitCommitReceipt {
        self.armed = false;
        SplitCommitReceipt::from_exact_parts(
            Arc::clone(&self.pane),
            self.registration.clone(),
            tab,
            window_id,
            size,
        )
    }

    fn commit_tab(mut self, tab: Arc<Tab>) -> Arc<Tab> {
        self.armed = false;
        tab
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

fn prepare_registered_pane(mux: &Arc<Mux>, pane: &Arc<dyn Pane>) -> anyhow::Result<PreparedPane> {
    let registration = match mux.capture_pane_registration(pane) {
        Some(registration) => registration,
        None => {
            let rollback = catch_recoverable(
                RecoverablePanicSite::MuxRegistrationRollback,
                std::panic::AssertUnwindSafe(|| pane.kill()),
            );
            if rollback.is_err() {
                log::error!(
                    "unregistered spawned-pane rollback panicked for exact pane identity {:p}",
                    Arc::as_ptr(pane)
                );
            }
            anyhow::bail!(
                "spawned pane has no exact mux registration; rolled back the unregistered pane"
            );
        }
    };
    Ok(PreparedPane::new(Arc::clone(pane), registration))
}

impl Drop for PreparedPane {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| self.registration.retire_if_current()),
        );
        match rollback {
            Ok(true) => {}
            Ok(false) => {
                log::warn!(
                    "prepared pane {} lost exact registration before rollback",
                    self.registration.pane_id()
                );
            }
            Err(_) => {
                log::error!(
                    "prepared pane {} rollback panicked",
                    self.registration.pane_id()
                );
            }
        }
    }
}

// `async_trait` must synthesize boxed `Future` return values to keep this
// trait object-safe. Those futures are already `#[must_use]`; the macro also
// annotates each generated trait method, which newer Clippy diagnoses as
// `double_must_use` even though there is no source-level attribute to remove.
// Scope the compatibility allowance to this one macro-generated trait surface;
// placing `#[expect]` outside the macro expansion is itself unfulfillable.
#[allow(
    clippy::double_must_use,
    reason = "async_trait duplicates the intrinsic must-use contract of its generated boxed futures"
)]
#[async_trait(?Send)]
pub trait Domain: Downcast + Send + Sync {
    /// Spawn a new command within this domain on the exact originating mux.
    async fn spawn(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let pane = self
            .spawn_pane(mux, size, command, command_dir)
            .await
            .context("spawn")?;
        let prepared = prepare_registered_pane(mux, &pane)?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);

        mux.add_tab_and_active_pane(&tab)?;
        if let Err(error) = mux.add_tab_to_window(&tab, window) {
            if mux
                .remove_tab_internal_if_same_with_pane_disposition(&tab, true)
                .is_some()
            {
                prepared.disarm();
            }
            return Err(error);
        }

        Ok(prepared.commit_tab(tab))
    }

    /// Spawn a new pane and commit it beside the exact admitted target.
    async fn split_pane_spawned(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        split_request: SplitRequest,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux),
            "split target belongs to another mux registration"
        );
        let (_domain_id, window_id, tab) = target.exact_location()?;
        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|positioned| Arc::ptr_eq(&positioned.pane, target.pane()))
        {
            Some(p) => p.index,
            None => anyhow::bail!(
                "exact split target registration {} is not tiled",
                target.pane_id()
            ),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let target_config = target.with_pane(|pane| pane.get_config());
        let pane = self
            .spawn_pane(mux, split_size.second, command, command_dir)
            .await?;
        let prepared = prepare_registered_pane(mux, &pane)?;
        if let Some(config) = target_config {
            pane.set_config(config);
        }
        let dims = pane.get_dimensions();
        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: dims.pixel_height,
            pixel_width: dims.pixel_width,
            dpi: dims.dpi,
        };
        tab.split_and_insert(pane_index, split_request, pane)?;
        Ok(prepared.commit_split(tab, window_id, size))
    }

    /// Move another exact registration into a split beside the target.
    async fn split_pane_moved(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        source: &PaneOperationGuard,
        split_request: SplitRequest,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux) && source.belongs_to(mux),
            "split source and target must belong to the originating mux"
        );
        anyhow::ensure!(
            !target.same_registration(source),
            "cannot move pane {} into a split of itself",
            target.pane_id()
        );
        mux.commit_guarded_moved_split(target, source, split_request)
    }

    /// Spawn and register a pane on the exact originating mux.
    async fn spawn_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>>;

    /// Construct a pane process/PTY without publishing it in a mux.
    ///
    /// Only domains that can uphold the unpublished reservation contract may
    /// override this method. The returned guard kills the pane on cancellation
    /// or any other pre-publication failure until it is consumed by mux code.
    async fn spawn_unpublished_pane(
        &self,
        _mux: &Arc<Mux>,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<UnpublishedPane> {
        bail!(
            "domain `{}` does not support unpublished pane construction",
            self.domain_name()
        )
    }

    /// The mux will call this method on the domain of the pane that
    /// is being moved to give the domain a chance to handle the movement.
    /// `mux` is the exact originating mux that admitted that movement.
    /// If this method returns Ok(None), then the mux will handle the
    /// movement itself by mutating its local Tabs and Windows.
    async fn move_pane_to_new_tab(
        &self,
        _mux: &Arc<Mux>,
        _pane: &PaneOperationGuard,
        _window_id: Option<WindowId>,
        _workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<MoveCommitReceipt>> {
        Ok(None)
    }

    /// Returns false if the `spawn` method will never succeed.
    /// There are some internal placeholder domains that are
    /// pre-created with local UI that we do not want to allow
    /// to show in the launcher/menu as launchable items.
    fn spawnable(&self) -> bool {
        true
    }

    /// Whether this domain can authoritatively spawn directly into a local
    /// floating layer without first publishing a normal tab.
    ///
    /// Client-backed and asynchronously materialized remote domains must leave
    /// this false until their protocol exposes an atomic remote operation. The
    /// mux checks this synchronously before attach or spawn.
    fn supports_floating_pane_spawn(&self) -> bool {
        false
    }

    /// Returns true if the `detach` method can be used
    /// to detach the domain, preserving the associated
    /// panes, or false if the `detach` method will never
    /// succeed
    fn detachable(&self) -> bool;

    /// Returns the domain id, which is useful for obtaining
    /// a handle on the domain later.
    fn domain_id(&self) -> DomainId;

    /// Returns the name of the domain.
    /// Should be a short identifier.
    fn domain_name(&self) -> &str;

    /// Returns a label describing the domain.
    async fn domain_label(&self) -> String {
        self.domain_name().to_string()
    }

    /// Re-attach to any tabs that might be pre-existing in this domain
    /// Attach to this domain using mux and client authority captured before
    /// any asynchronous connection work begins.
    async fn attach(
        &self,
        mux: &Arc<Mux>,
        owner_client_id: Option<Arc<ClientId>>,
        window_id: Option<WindowId>,
    ) -> anyhow::Result<()>;

    /// Detach all tabs
    fn detach(&self) -> anyhow::Result<()>;

    /// Indicates the state of the domain
    fn state(&self) -> DomainState;
}
impl_downcast!(Domain);

pub struct LocalDomain {
    pty_system: Mutex<Box<dyn PtySystem + Send>>,
    id: DomainId,
    name: String,
}

impl LocalDomain {
    pub fn new(name: &str) -> Result<Self, Error> {
        Ok(Self::with_pty_system(name, native_pty_system()))
    }

    fn resolve_exec_domain(&self) -> Option<ExecDomain> {
        config::configuration()
            .exec_domains
            .iter()
            .find(|ed| ed.name == self.name)
            .cloned()
    }

    fn resolve_wsl_domain(&self) -> Option<WslDomain> {
        config::configuration()
            .wsl_domains()
            .iter()
            .find(|d| d.name == self.name)
            .cloned()
    }

    pub fn with_pty_system(name: &str, pty_system: Box<dyn PtySystem + Send>) -> Self {
        let id = alloc_domain_id();
        Self {
            pty_system: Mutex::new(pty_system),
            id,
            name: name.to_string(),
        }
    }

    pub fn new_wsl(wsl: WslDomain) -> Result<Self, Error> {
        Self::new(&wsl.name)
    }

    pub fn new_exec_domain(exec_domain: ExecDomain) -> anyhow::Result<Self> {
        Self::new(&exec_domain.name)
    }

    pub fn new_serial_domain(serial_domain: SerialDomain) -> anyhow::Result<Self> {
        let port = serial_domain.port.as_ref().unwrap_or(&serial_domain.name);
        let mut serial = portable_pty::serial::SerialTty::new(&port);
        if let Some(baud) = serial_domain.baud {
            let baud_u32: u32 = baud
                .try_into()
                .map_err(|_| anyhow::anyhow!("baud rate {} exceeds u32::MAX", baud))?;
            serial.set_baud_rate(baud_u32);
        }
        let pty_system = Box::new(serial);
        Ok(Self::with_pty_system(&serial_domain.name, pty_system))
    }

    fn wslenv_entry_name(entry: &str) -> &str {
        entry.split_once('/').map(|(name, _)| name).unwrap_or(entry)
    }

    fn augment_wslenv_for_wsl_command(cmd: &mut CommandBuilder) {
        let mut wslenv_entries: Vec<String> = cmd
            .get_env("WSLENV")
            .map(|value| value.to_string_lossy().to_string())
            .map(|value| {
                value
                    .split(':')
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        for (key, _) in cmd.iter_extra_env_as_str() {
            if key == "WSLENV" {
                continue;
            }
            if wslenv_entries
                .iter()
                .any(|entry| Self::wslenv_entry_name(entry) == key)
            {
                continue;
            }
            wslenv_entries.push(key.to_string());
        }

        if !wslenv_entries.is_empty() {
            cmd.env("WSLENV", wslenv_entries.join(":"));
        }
    }

    fn rewrite_command_for_wsl(cmd: &mut CommandBuilder, wsl: &WslDomain) -> anyhow::Result<()> {
        let mut args: Vec<OsString> = cmd.get_argv().clone();

        if args.is_empty() {
            if let Some(def_prog) = &wsl.default_prog {
                for arg in def_prog {
                    args.push(arg.into());
                }
            }
        }

        let mut argv: Vec<OsString> = vec![
            "wsl.exe".into(),
            "--distribution".into(),
            wsl.distribution
                .as_deref()
                .unwrap_or(wsl.name.as_str())
                .into(),
        ];

        if let Some(cwd) = cmd.get_cwd() {
            argv.push("--cd".into());
            argv.push(cwd.into());
        }

        if let Some(user) = &wsl.username {
            argv.push("--user".into());
            argv.push(user.into());
        }

        if !args.is_empty() {
            argv.push("--exec".into());
            for arg in args {
                argv.push(arg);
            }
        }

        // WSL only imports Windows-side environment variables that are
        // listed in WSLENV, so copy explicit command env keys into the
        // existing WSLENV contract before we swap argv to `wsl.exe --exec`.
        Self::augment_wslenv_for_wsl_command(cmd);

        cmd.clear_cwd();
        *cmd.get_argv_mut() = argv;
        Ok(())
    }

    #[cfg(unix)]
    fn is_conpty(&self) -> bool {
        false
    }

    #[cfg(windows)]
    fn is_conpty(&self) -> bool {
        let pty_system = self.pty_system.lock();
        let pty_system: &dyn PtySystem = &**pty_system;
        pty_system
            .downcast_ref::<portable_pty::win::conpty::ConPtySystem>()
            .is_some()
    }

    async fn fixup_command(&self, cmd: &mut CommandBuilder) -> anyhow::Result<()> {
        if let Some(wsl) = self.resolve_wsl_domain() {
            Self::rewrite_command_for_wsl(cmd, &wsl)?;
        } else if let Some(ed) = self.resolve_exec_domain() {
            let mut args = vec![];
            let mut set_environment_variables = HashMap::new();
            for arg in cmd.get_argv() {
                args.push(
                    arg.to_str()
                        .ok_or_else(|| anyhow::anyhow!("command argument is not utf8"))?
                        .to_string(),
                );
            }
            for (k, v) in cmd.iter_full_env_as_str() {
                set_environment_variables.insert(k.to_string(), v.to_string());
            }
            let cwd = match cmd.get_cwd() {
                Some(cwd) => Some(PathBuf::from(cwd)),
                None => None,
            };
            let spawn_command = SpawnCommand {
                label: None,
                domain: SpawnTabDomain::DomainName(ed.name.clone()),
                args: if args.is_empty() { None } else { Some(args) },
                set_environment_variables,
                cwd,
                position: None,
            };

            #[cfg(feature = "lua")]
            let spawn_command = config::with_lua_config_on_main_thread(|lua| async {
                let lua = lua.ok_or_else(|| anyhow::anyhow!("missing lua context"))?;
                let value = config::lua::emit_async_callback(
                    &*lua,
                    (ed.fixup_command.clone(), (spawn_command.clone())),
                )
                .await?;
                let cmd: SpawnCommand =
                    luahelper::from_lua_value_dynamic(value).with_context(|| {
                        format!(
                            "interpreting SpawnCommand result from ExecDomain {}",
                            ed.name
                        )
                    })?;
                Ok(cmd)
            })
            .await
            .with_context(|| format!("calling ExecDomain {} function", ed.name))?;
            #[cfg(not(feature = "lua"))]
            let spawn_command = spawn_command;

            // Reinterpret the SpawnCommand into the builder

            cmd.get_argv_mut().clear();
            if let Some(args) = &spawn_command.args {
                for arg in args {
                    cmd.get_argv_mut().push(arg.into());
                }
            }
            cmd.env_clear();
            for (k, v) in &spawn_command.set_environment_variables {
                cmd.env(k, v);
            }
            cmd.clear_cwd();
            if let Some(cwd) = &spawn_command.cwd {
                cmd.cwd(cwd);
            }
        } else if Path::new("/.flatpak-info").exists() {
            // We're running inside a flatpak sandbox.
            // Run the command outside the sandbox via flatpak-spawn
            let mut args = vec![
                "flatpak-spawn".to_string(),
                "--host".to_string(),
                "--watch-bus".to_string(),
            ];
            if let Some(cwd) = cmd.get_cwd() {
                args.push(format!("--directory={}", Path::new(cwd).display()));
            }

            let is_default_prog = cmd.is_default_prog();

            // Note: WEZTERM_UNIX_SOCKET, WEZTERM_CONFIG_(FILE|DIR) and other env
            // vars are not included in this.
            // We can't include them: their paths are only meaningful in the sandbox
            // and cannot be reasonably accessed from outside it in the shell.
            for (k, v) in cmd.iter_extra_env_as_str() {
                args.push(format!("--env={k}={v}"));
            }

            for arg in cmd.get_argv() {
                args.push(
                    arg.to_str()
                        .ok_or_else(|| anyhow::anyhow!("command argument is not utf8"))?
                        .to_string(),
                );
            }

            if is_default_prog {
                // We can't read $SHELL from inside the sandbox, so ask the host.
                //
                // Guard both failure modes the old code swallowed silently:
                //   * non-zero exit from flatpak-spawn (missing host socket,
                //     --host denied, etc.) — the earlier code dropped stderr
                //     and still pushed stdout (often empty) as the shell
                //     argument, producing a malformed `flatpak-spawn` argv
                //     whose downstream spawn_command failure carried no
                //     context pointing at the real cause;
                //   * empty `$SHELL` — `echo $SHELL` can legitimately
                //     return just `\n` if the host shell has no SHELL set,
                //     in which case we'd push `""` as a program name and
                //     exec() would fail with an opaque ENOENT.
                let output = std::process::Command::new("flatpak-spawn")
                    .args(["--host", "sh", "-c", "echo $SHELL"])
                    .output()
                    .context("invoking flatpak-spawn --host sh -c 'echo $SHELL'")?;
                if !output.status.success() {
                    anyhow::bail!(
                        "flatpak-spawn --host sh -c 'echo $SHELL' failed (status={:?}): {}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr).trim(),
                    );
                }
                let shell = String::from_utf8_lossy(&output.stdout);
                let shell = shell.trim();
                if shell.is_empty() {
                    anyhow::bail!(
                        "flatpak-spawn --host reported an empty $SHELL; \
                         set SHELL on the host or spawn an explicit program"
                    );
                }

                args.push(shell.to_string());
                // Assume we can pass `-l` for a login shell
                args.push("-l".to_string());
            }

            // Avoid setting up the controlling tty as that is not compatible
            // with flatpak:
            // <https://github.com/flatpak/flatpak/issues/3697>
            // <https://github.com/flatpak/flatpak/issues/3285>
            cmd.set_controlling_tty(false);

            // Re-apply to the builder
            cmd.get_argv_mut().clear();
            for arg in args {
                cmd.get_argv_mut().push(arg.into());
            }
            cmd.clear_cwd();
            log::trace!("made: {cmd:#?}");
        } else if let Some(dir) = cmd.get_cwd() {
            // I'm not normally a fan of existence checking, but not checking here
            // can be painful; in the case where a tab is local but has connected
            // to a remote system and that remote has used OSC 7 to set a path
            // that doesn't exist on the local system, process spawning can fail.
            // Another situation is `sudo -i` has the pane with set to a cwd
            // that is not accessible to the user.
            if let Err(err) = Path::new(&dir).read_dir() {
                log::warn!(
                    "Directory {:?} is not readable and will not be \
                     used for the command we are spawning: {:#}",
                    dir,
                    err
                );
                cmd.clear_cwd();
            }
        }
        Ok(())
    }

    async fn build_command(
        &self,
        mux: &Arc<Mux>,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        pane_id: PaneId,
    ) -> anyhow::Result<CommandBuilder> {
        let config = configuration();

        let wsl = self.resolve_wsl_domain();
        let default_prog = wsl
            .as_ref()
            .map(|wsl| wsl.default_prog.as_ref())
            .unwrap_or(config.default_prog.as_ref());

        let mut cmd = match command {
            Some(mut cmd) => {
                config.apply_cmd_defaults(&mut cmd, default_prog, config.default_cwd.as_ref());
                cmd
            }
            None => config.build_prog(
                None,
                default_prog,
                wsl.as_ref()
                    .map(|wsl| wsl.default_cwd.as_ref())
                    .unwrap_or(config.default_cwd.as_ref()),
            )?,
        };
        if let Some(dir) = command_dir {
            cmd.cwd(dir);
        }
        if let Ok(sock) = std::env::var("WEZTERM_UNIX_SOCKET") {
            cmd.env("WEZTERM_UNIX_SOCKET", sock);
        }
        cmd.env("WEZTERM_PANE", pane_id.to_string());
        if let Some(agent_path) = mux.agent.as_ref().map(|agent| agent.path().to_path_buf()) {
            cmd.env("SSH_AUTH_SOCK", agent_path);
        }
        self.fixup_command(&mut cmd).await?;
        Ok(cmd)
    }
}

/// Allows sharing the writer between the Pane and the Terminal.
/// This could potentially be eliminated in the future if we can
/// teach the Pane impl to reference the writer in the Termninal,
/// but the Pane trait returns a RefMut and that makes it a bit
/// awkward at the moment.
#[derive(Clone)]
pub(crate) struct WriterWrapper {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl WriterWrapper {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }
}

impl std::io::Write for WriterWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.lock().flush()
    }
}

/// Wraps the underlying pty; we use this as a marker for when
/// the spawn attempt failed in order to hold the pane open
pub(crate) struct FailedSpawnPty {
    inner: Mutex<Box<dyn MasterPty>>,
}

impl portable_pty::MasterPty for FailedSpawnPty {
    fn resize(&self, new_size: PtySize) -> anyhow::Result<()> {
        self.inner.lock().resize(new_size)
    }
    fn get_size(&self) -> anyhow::Result<PtySize> {
        self.inner.lock().get_size()
    }
    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send + 'static>> {
        self.inner.lock().try_clone_reader()
    }
    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send + 'static>> {
        self.inner.lock().take_writer()
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> {
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A fake child process for the case where the spawn attempt
/// failed. It reports as immediately terminated.
#[derive(Debug)]
pub(crate) struct FailedProcessSpawn {}

impl portable_pty::Child for FailedProcessSpawn {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Ok(Some(ExitStatus::with_exit_code(1)))
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Ok(ExitStatus::with_exit_code(1))
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

impl portable_pty::ChildKiller for FailedProcessSpawn {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(FailedProcessSpawn {})
    }
}

/// Owns the result of the blocking process-spawn worker until the awaiting
/// future has actually claimed it.
///
/// Dropping the receiver side of `spawn_into_new_thread` does not stop its OS
/// thread. Without this guard, a successful child produced after async
/// cancellation would be dropped alive when the worker discovered that its
/// result receiver was gone. Keeping the result armed in the channel makes
/// cancellation kill that otherwise-orphaned child exactly once.
struct KillOnDropChildResult {
    result: Option<anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl KillOnDropChildResult {
    fn new(result: anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>>) -> Self {
        Self {
            result: Some(result),
        }
    }

    fn into_result(mut self) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        self.result
            .take()
            .expect("child spawn result consumed more than once")
    }
}

impl Drop for KillOnDropChildResult {
    fn drop(&mut self) {
        let Some(Ok(mut child)) = self.result.take() else {
            return;
        };
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| child.kill()),
        );
        match rollback {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!(
                    "cancelled local pane spawn could not kill its unclaimed child: {error}"
                );
            }
            Err(_) => {
                log::error!(
                    "cancelled local pane spawn panicked while killing its unclaimed child"
                );
            }
        }
    }
}

#[async_trait(?Send)]
impl Domain for LocalDomain {
    async fn spawn_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let unpublished = self
            .spawn_unpublished_pane(mux, size, command, command_dir)
            .await?;
        mux.add_pane(unpublished.pane())?;
        Ok(unpublished.into_pane())
    }

    async fn spawn_unpublished_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<UnpublishedPane> {
        let pane_id = alloc_pane_id()?;
        let cmd = self
            .build_command(mux, command, command_dir, pane_id)
            .await
            .context("build_command")?;
        let pair = self
            .pty_system
            .lock()
            .openpty(crate::terminal_size_to_pty_size(size)?)?;

        let command_line = cmd
            .as_unix_command_line()
            .unwrap_or_else(|err| format!("error rendering command line: {:?}", err));
        let command_description = format!(
            "\"{}\" in domain \"{}\"",
            if command_line.is_empty() {
                cmd.get_shell()
            } else {
                command_line
            },
            self.name
        );
        // [ft-s9oci] `SlavePty::spawn_command` wraps `fork + exec` on Unix
        // (and `CreateProcess` on Windows). Both are fully synchronous
        // syscall sequences that can stall for a long time — a slow
        // program load, a hung NSS lookup reaching /etc/nsswitch.conf,
        // an LD_PRELOAD hook that deadlocks in its own init. Calling it
        // directly from this async fn parks the executor thread until
        // the child is either up or the call fails; every other async
        // task scheduled on that thread (IPC handlers, tmux event
        // processing, UI repaint) stops making progress too.
        //
        // Hand the fork off to a dedicated OS thread via
        // `promise::spawn::spawn_into_new_thread`. The slave pty and
        // the CommandBuilder are both `Send`, and the slave is not
        // referenced anywhere else on this code path after spawn —
        // dropping it inside the worker thread once the spawn settles
        // is correct. The worker returns a kill-on-drop result reservation.
        // If this async future is cancelled while the syscall sequence is
        // still blocked, the channel eventually drops that reservation and
        // kills any child which materialized after cancellation.
        let PtyPair { slave, master } = pair;
        let guarded_child_result = promise::spawn::spawn_into_new_thread(move || {
            Ok(KillOnDropChildResult::new(slave.spawn_command(cmd)))
        })
        .await;
        let mut writer = WriterWrapper::new(master.take_writer()?);

        let term_config =
            config::TermConfig::new_for_pane(pane_id, self.id, command_description.clone());
        let mut terminal = frankenterm_term::Terminal::new(
            size,
            std::sync::Arc::new(term_config),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );
        if self.is_conpty() {
            terminal.enable_conpty_quirks();
        }

        // Keep the child result reservation armed through every fallible setup
        // step above. Only the immediate LocalPane construction below takes
        // ownership away from its kill-on-drop guard.
        let child_result = match guarded_child_result {
            Ok(result) => result.into_result(),
            Err(error) => Err(error),
        };

        let pane: Arc<dyn Pane> = match child_result {
            Ok(child) => Arc::new(LocalPane::new(
                pane_id,
                terminal,
                child,
                master,
                Box::new(writer),
                self.id,
                command_description,
            )),
            Err(err) => {
                // Show the error to the user in the new pane
                let display_result = write!(writer, "{err:#}").and_then(|_| writer.flush());
                if display_result.is_err() {
                    tracing::warn!(
                        spawn_error = ?err,
                        pane_error_display = ?display_result,
                        "failed to surface local pane spawn error"
                    );
                }

                // and return a dummy pane that has exited
                Arc::new(LocalPane::new(
                    pane_id,
                    terminal,
                    Box::new(FailedProcessSpawn {}),
                    Box::new(FailedSpawnPty {
                        inner: Mutex::new(master),
                    }),
                    Box::new(writer),
                    self.id,
                    command_description,
                ))
            }
        };

        Ok(UnpublishedPane::new(pane))
    }

    fn supports_floating_pane_spawn(&self) -> bool {
        true
    }

    fn domain_id(&self) -> DomainId {
        self.id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn domain_label(&self) -> String {
        if let Some(ed) = self.resolve_exec_domain() {
            match &ed.label {
                Some(ValueOrFunc::Value(frankenterm_dynamic::Value::String(s))) => s.to_string(),
                Some(ValueOrFunc::Func(label_func)) => {
                    #[cfg(feature = "lua")]
                    {
                        let label = config::with_lua_config_on_main_thread(|lua| async {
                            let lua = lua.ok_or_else(|| anyhow::anyhow!("missing lua context"))?;
                            let value = config::lua::emit_async_callback(
                                &*lua,
                                (label_func.clone(), (self.name.clone())),
                            )
                            .await?;
                            let label: String = luahelper::from_lua_value_dynamic(value)
                                .with_context(|| {
                                    format!(
                                        "interpreting SpawnCommand result from ExecDomain {}",
                                        ed.name
                                    )
                                })?;
                            Ok(label)
                        })
                        .await;
                        match label {
                            Ok(label) => label,
                            Err(err) => {
                                log::error!(
                                    "Error while calling label function for ExecDomain `{}`: {err:#}",
                                    self.name
                                );
                                self.name.to_string()
                            }
                        }
                    }
                    #[cfg(not(feature = "lua"))]
                    {
                        let _ = label_func;
                        self.name.to_string()
                    }
                }
                _ => self.name.to_string(),
            }
        } else if let Some(wsl) = self.resolve_wsl_domain() {
            wsl.distribution.unwrap_or_else(|| self.name.to_string())
        } else {
            self.name.to_string()
        }
    }

    async fn attach(
        &self,
        _mux: &Arc<Mux>,
        _owner_client_id: Option<Arc<ClientId>>,
        _window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        false
    }

    fn detach(&self) -> anyhow::Result<()> {
        bail!(
            "detach is unsupported for LocalDomain because local panes are owned by the current mux session"
        );
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{Child, ChildKiller, SlavePty};
    use std::future::{poll_fn, Future};
    use std::io::{Read, Result as IoResult, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
    use std::task::Poll;
    use std::time::{Duration, Instant};

    fn mux_test_lock() -> &'static StdMutex<()> {
        &crate::MUX_TEST_LOCK
    }

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: Arc<Mux>) -> Self {
            let guard = mux_test_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self {
                prior,
                _guard: guard,
            }
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    #[derive(Debug, Clone)]
    struct TestChild {
        kill_calls: Option<Arc<AtomicUsize>>,
    }

    impl TestChild {
        fn untracked() -> Self {
            Self { kill_calls: None }
        }

        fn tracked(kill_calls: Arc<AtomicUsize>) -> Self {
            Self {
                kill_calls: Some(kill_calls),
            }
        }
    }

    impl ChildKiller for TestChild {
        fn kill(&mut self) -> IoResult<()> {
            if let Some(kill_calls) = &self.kill_calls {
                kill_calls.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for TestChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> IoResult<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(4242)
        }
    }

    #[derive(Clone, Default)]
    struct BufferWriter {
        written: Arc<StdMutex<Vec<u8>>>,
    }

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            self.written
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> IoResult<()> {
            Ok(())
        }
    }

    struct TestMasterPty {
        written: Arc<StdMutex<Vec<u8>>>,
    }

    impl TestMasterPty {
        fn new() -> Self {
            Self {
                written: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    impl MasterPty for TestMasterPty {
        fn resize(&self, _size: PtySize) -> Result<(), Error> {
            Ok(())
        }

        fn get_size(&self) -> Result<PtySize, Error> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, Error> {
            struct BlockingReader;
            impl Read for BlockingReader {
                fn read(&mut self, _buf: &mut [u8]) -> IoResult<usize> {
                    std::thread::sleep(Duration::from_secs(86400));
                    Ok(0)
                }
            }
            Ok(Box::new(BlockingReader))
        }

        fn take_writer(&self) -> Result<Box<dyn Write + Send>, Error> {
            Ok(Box::new(BufferWriter {
                written: Arc::clone(&self.written),
            }))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    struct SlowSpawnSlavePty {
        delay: Duration,
        spawn_calls: Arc<AtomicUsize>,
    }

    impl SlavePty for SlowSpawnSlavePty {
        fn spawn_command(
            &self,
            _cmd: CommandBuilder,
        ) -> Result<Box<dyn Child + Send + Sync>, Error> {
            self.spawn_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            Ok(Box::new(TestChild::untracked()))
        }
    }

    struct SlowSpawnPtySystem {
        delay: Duration,
        spawn_calls: Arc<AtomicUsize>,
    }

    impl SlowSpawnPtySystem {
        fn new(delay: Duration) -> (Self, Arc<AtomicUsize>) {
            let spawn_calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    delay,
                    spawn_calls: Arc::clone(&spawn_calls),
                },
                spawn_calls,
            )
        }
    }

    impl PtySystem for SlowSpawnPtySystem {
        fn openpty(&self, _size: PtySize) -> anyhow::Result<PtyPair> {
            Ok(PtyPair {
                slave: Box::new(SlowSpawnSlavePty {
                    delay: self.delay,
                    spawn_calls: Arc::clone(&self.spawn_calls),
                }),
                master: Box::new(TestMasterPty::new()),
            })
        }
    }

    struct CancellationSpawnSlavePty {
        started: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        kill_calls: Arc<AtomicUsize>,
    }

    impl SlavePty for CancellationSpawnSlavePty {
        fn spawn_command(
            &self,
            _cmd: CommandBuilder,
        ) -> Result<Box<dyn Child + Send + Sync>, Error> {
            self.started.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(Box::new(TestChild::tracked(Arc::clone(&self.kill_calls))))
        }
    }

    struct CancellationSpawnPtySystem {
        started: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        kill_calls: Arc<AtomicUsize>,
    }

    impl PtySystem for CancellationSpawnPtySystem {
        fn openpty(&self, _size: PtySize) -> anyhow::Result<PtyPair> {
            Ok(PtyPair {
                slave: Box::new(CancellationSpawnSlavePty {
                    started: Arc::clone(&self.started),
                    release: Arc::clone(&self.release),
                    kill_calls: Arc::clone(&self.kill_calls),
                }),
                master: Box::new(TestMasterPty::new()),
            })
        }
    }

    fn wslenv_entries(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_env("WSLENV")
            .map(|value| value.to_string_lossy().to_string())
            .map(|value| {
                value
                    .split(':')
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn domain_state_equality() {
        assert_eq!(DomainState::Detached, DomainState::Detached);
        assert_eq!(DomainState::Attached, DomainState::Attached);
        assert_ne!(DomainState::Detached, DomainState::Attached);
    }

    #[test]
    fn local_domain_detach_is_explicitly_unsupported() {
        let domain = LocalDomain::new("local-detach-test").expect("local domain");
        assert!(!domain.detachable());

        let err = domain
            .detach()
            .expect_err("local domain detach should fail");
        let err = err.to_string();
        assert!(err.contains("unsupported"), "{}", err);
        assert!(err.contains("LocalDomain"), "{}", err);
    }

    #[test]
    fn domain_state_clone_copy() {
        let s = DomainState::Attached;
        let s2 = s; // Copy
        let s3 = s.clone(); // Clone
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    #[test]
    fn domain_state_debug() {
        let dbg = format!("{:?}", DomainState::Detached);
        assert!(dbg.contains("Detached"));
        let dbg = format!("{:?}", DomainState::Attached);
        assert!(dbg.contains("Attached"));
    }

    #[test]
    fn split_source_move_pane() {
        let a = SplitSource::MovePane(42);
        let b = SplitSource::MovePane(42);
        assert_eq!(a, b);

        let c = SplitSource::MovePane(99);
        assert_ne!(a, c);
    }

    #[test]
    fn split_source_spawn_no_command() {
        let a = SplitSource::Spawn {
            command: None,
            command_dir: None,
        };
        let b = SplitSource::Spawn {
            command: None,
            command_dir: None,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn split_source_spawn_with_dir() {
        let a = SplitSource::Spawn {
            command: None,
            command_dir: Some("/home/user".to_string()),
        };
        let b = SplitSource::Spawn {
            command: None,
            command_dir: Some("/home/user".to_string()),
        };
        assert_eq!(a, b);

        let c = SplitSource::Spawn {
            command: None,
            command_dir: Some("/tmp".to_string()),
        };
        assert_ne!(a, c);
    }

    #[test]
    fn split_source_debug() {
        let s = SplitSource::MovePane(5);
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("MovePane"));
        assert!(dbg.contains("5"));
    }

    #[test]
    fn split_source_clone() {
        let a = SplitSource::MovePane(10);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn split_source_variants_inequality() {
        let spawn = SplitSource::Spawn {
            command: None,
            command_dir: None,
        };
        let mv = SplitSource::MovePane(0);
        assert_ne!(spawn, mv);
    }

    #[test]
    fn rewrite_command_for_wsl_adds_explicit_env_keys_to_wslenv() -> anyhow::Result<()> {
        let wsl = WslDomain {
            name: "WSL:Ubuntu".to_string(),
            distribution: Some("Ubuntu".to_string()),
            username: Some("alice".to_string()),
            default_cwd: None,
            default_prog: None,
        };

        let mut cmd = CommandBuilder::new("bash");
        cmd.cwd("/tmp/project");
        cmd.env("WSLENV", "TERM:COLORTERM");
        cmd.env("WEZTERM_PANE", "7");
        cmd.env("CUSTOM_KEY", "custom");

        LocalDomain::rewrite_command_for_wsl(&mut cmd, &wsl)?;

        let argv = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            argv,
            vec![
                "wsl.exe",
                "--distribution",
                "Ubuntu",
                "--cd",
                "/tmp/project",
                "--user",
                "alice",
                "--exec",
                "bash",
            ]
        );
        assert!(cmd.get_cwd().is_none());

        let mut entries = wslenv_entries(&cmd);
        entries.sort();
        assert_eq!(
            entries,
            vec!["COLORTERM", "CUSTOM_KEY", "TERM", "WEZTERM_PANE"]
        );
        Ok(())
    }

    #[test]
    fn rewrite_command_for_wsl_preserves_existing_flagged_wslenv_entries() -> anyhow::Result<()> {
        let wsl = WslDomain {
            name: "WSL:Ubuntu".to_string(),
            distribution: Some("Ubuntu".to_string()),
            username: None,
            default_cwd: None,
            default_prog: None,
        };

        let mut cmd = CommandBuilder::new("env");
        cmd.env("WSLENV", "SSH_AUTH_SOCK/p:TERM");
        cmd.env("SSH_AUTH_SOCK", "/tmp/agent.sock");
        cmd.env("TERM", "xterm-256color");
        cmd.env("WEZTERM_PANE", "11");

        LocalDomain::rewrite_command_for_wsl(&mut cmd, &wsl)?;

        let entries = wslenv_entries(&cmd);
        assert!(entries.iter().any(|entry| entry == "SSH_AUTH_SOCK/p"));
        assert!(entries.iter().any(|entry| entry == "TERM"));
        assert!(entries.iter().any(|entry| entry == "WEZTERM_PANE"));
        assert_eq!(
            entries
                .iter()
                .filter(|entry| LocalDomain::wslenv_entry_name(entry) == "SSH_AUTH_SOCK")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| LocalDomain::wslenv_entry_name(entry) == "TERM")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn local_domain_spawn_pane_first_poll_stays_non_blocking() {
        const SPAWN_DELAY: Duration = Duration::from_millis(200);

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let exec = promise::spawn::ScopedExecutor::new();
        let (pty_system, spawn_calls) = SlowSpawnPtySystem::new(SPAWN_DELAY);
        let domain = LocalDomain::with_pty_system("slow-spawn-test", Box::new(pty_system));

        let pane_id = promise::spawn::block_on(exec.run(async {
            let mut spawn_pane = std::pin::pin!(domain.spawn_pane(
                &mux,
                TerminalSize::default(),
                Some(CommandBuilder::new("slow-spawn-test")),
                None,
            ));

            let first_poll = poll_fn(|cx| {
                Poll::Ready(match spawn_pane.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;

            assert!(
                first_poll.is_none(),
                "[ft-odywh] spawn_pane completed during the first poll; \
                 spawn_command is likely running synchronously on the executor thread again"
            );

            spawn_pane
                .await
                .expect("spawn pane should succeed")
                .pane_id()
        }));

        assert_eq!(
            spawn_calls.load(Ordering::SeqCst),
            1,
            "[ft-odywh] fake PTY should be spawned exactly once"
        );
        assert!(
            mux.get_pane(pane_id).is_some(),
            "[ft-odywh] mux should register the pane returned by spawn_pane"
        );
    }

    #[test]
    fn cancelled_unpublished_spawn_kills_child_materialized_after_cancellation() {
        const WAIT_LIMIT: Duration = Duration::from_secs(2);

        struct ReleaseOnDrop(Arc<AtomicBool>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let exec = promise::spawn::ScopedExecutor::new();
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let kill_calls = Arc::new(AtomicUsize::new(0));
        let release_on_drop = ReleaseOnDrop(Arc::clone(&release));
        let domain = LocalDomain::with_pty_system(
            "cancelled-spawn-test",
            Box::new(CancellationSpawnPtySystem {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                kill_calls: Arc::clone(&kill_calls),
            }),
        );

        promise::spawn::block_on(exec.run(async {
            let mut spawn = domain.spawn_unpublished_pane(
                &mux,
                TerminalSize::default(),
                Some(CommandBuilder::new("cancelled-spawn-test")),
                None,
            );

            let first_poll = poll_fn(|cx| {
                Poll::Ready(match spawn.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            assert!(
                first_poll.is_none(),
                "unpublished spawn unexpectedly completed before its worker was released"
            );

            let deadline = Instant::now() + WAIT_LIMIT;
            while !started.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                started.load(Ordering::Acquire),
                "blocking spawn worker did not start within {wait_limit:?}",
                wait_limit = WAIT_LIMIT,
            );

            drop(spawn);
            release.store(true, Ordering::Release);
        }));
        drop(exec);
        drop(release_on_drop);

        let deadline = Instant::now() + WAIT_LIMIT;
        while kill_calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            kill_calls.load(Ordering::SeqCst),
            1,
            "a child produced after spawn cancellation must be killed exactly once"
        );
    }
}
