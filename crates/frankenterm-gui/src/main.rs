// Shared production implementation included by the distinct binary and test
// harness crate roots. Crate-level target attributes live in those
// wrappers because Cargo must not assign this same source path to two targets.

// Make `wezterm_dynamic` available as an alias for `frankenterm_dynamic`.
// Many vendored modules still use the old `wezterm_dynamic::` import paths.
extern crate frankenterm_dynamic as wezterm_dynamic;

use crate::customglyph::BlockKey;
use crate::glyphcache::GlyphCache;
use crate::utilsprites::RenderMetrics;
use ::window::*;
use anyhow::{Context, anyhow};
use clap::builder::ValueParser;
use clap::{Parser, ValueHint};
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::{ConfigHandle, SerialDomain, SshDomain, SshMultiplexing};
#[cfg(feature = "jemalloc")]
use frankenterm_alloc as _;
use frankenterm_client::domain::ClientDomain;
use frankenterm_core::macos_backend_select::{
    BackendOverride, BackendSelectionInputs, BackendSelectionResult, MacosArch, MacosVersion,
    select_macos_backend,
};
use frankenterm_font::FontConfiguration;
use frankenterm_font::shaper::PresentationWidth;
use frankenterm_mux_server_impl::update_mux_domains;
use frankenterm_toast_notification::*;
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain};
use mux::{DomainOperationGuard, Mux};
use mux_lua::MuxDomain;
use portable_pty::cmdbuilder::CommandBuilder;
use promise::spawn::block_on;
use std::borrow::Cow;
use std::collections::HashMap;
use std::env::{self, current_dir};
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use termwiz::cell::CellAttributes;
use termwiz::surface::{Line, SEQ_ZERO};
use unicode_normalization::UnicodeNormalization;
use wezterm_bidi::Direction;
use wezterm_gui_subcommands::*;

mod colorease;
mod commands;
mod customglyph;
mod dashboard;
mod download;
mod frontend;
mod glyphcache;
mod inputmap;
#[cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly"
    )
))]
mod native_bridge;
mod overlay;
mod quad;
mod renderstate;
mod resize_increment_calculator;
mod scripting;
mod scrollbar;
mod selection;
mod shapecache;
mod smart_selection_a11y;
mod spawn;
mod stats;
mod tabbar;
mod termwindow;
// br-ft-wysai: `unicode_names.rs` is a 5.8MB / 140K-line generated
// table (`ucd-generate names .`, Unicode 16.0.0). Single consumer is
// `termwindow/charselect.rs` (character-picker fuzzy-search). Cold
// rebuilds incur a parse + typecheck pass over the whole file; repo
// clones carry the 5.8MB. If a future incident makes either of those
// load-bearing, candidate remediations (per the bead body):
//   A) extract to a `frankenterm-gui-unicode-names` const-only crate
//      so GUI changes don't trigger this file's recompile;
//   B) encode the table as a binary asset + lazy-load via phf/fst +
//      include_bytes! — same runtime mem, smaller compile + repo;
//   C) accept current trade-off (matches the unicode-segmentation /
//      idna ecosystem precedent).
// No action required today; this comment is the breadcrumb for
// future cold-build / repo-size incidents.
mod unicode_names;
mod uniforms;
mod update;
mod utilsprites;
use frankenterm_gui::{
    domain_reconnect_manifest,
    domain_reconnect_manifest::DomainAttachmentIntent,
    window_state_persist,
};

static AUTO_CONNECT_ENABLED: AtomicBool = AtomicBool::new(false);
static AUTO_CONNECT_STARTUP_READY: AtomicBool = AtomicBool::new(false);
static AUTO_CONNECT_SUPERVISOR_GENERATION: AtomicU64 = AtomicU64::new(0);
thread_local! {
    /// One process-local owner for the retry task. Replacing this handle drops
    /// and cancels the old future immediately, releasing both its exact domain
    /// operation guard and its task-lifetime scheduler admission permit.
    static AUTO_CONNECT_SUPERVISOR_TASK: std::cell::RefCell<Option<promise::spawn::Task<()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

// `dhat-heap` and `jemalloc` each define a `#[global_allocator]` — having
// both active produces a duplicate-static link error. Default builds enable
// `jemalloc`; profile with dhat by passing
//   --no-default-features --features dhat-heap.
// See ft-tkxpi.
#[cfg(all(feature = "dhat-heap", feature = "jemalloc"))]
compile_error!(
    "Features `dhat-heap` and `jemalloc` are mutually exclusive: \
     each defines #[global_allocator]. \
     Use `cargo build --no-default-features --features dhat-heap` to profile with dhat."
);

pub use selection::SelectionMode;
pub use termwindow::{ICON_DATA, TermWindow, set_window_class, set_window_position};

// ---------------------------------------------------------------------------
// Bootstrap (inlined from env-bootstrap, minus Lua registration)
// ---------------------------------------------------------------------------

const FT_MACOS_BACKEND_ENV: &str = "FT_MACOS_BACKEND";
const FT_ATOMIC_COMPONENT_MARKER: &str = env!("FT_ATOMIC_COMPONENT_MARKER");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GuiMacosBackendSelection {
    override_: BackendOverride,
    arch: MacosArch,
    version: MacosVersion,
    result: BackendSelectionResult,
}

fn frankenterm_bootstrap() {
    // Initialize logging from RUST_LOG env var
    env_logger::init();
    log_gui_macos_backend_selection();

    config::assign_version_info(
        concat!("FrankenTerm ", env!("CARGO_PKG_VERSION")),
        env!("FRANKENTERM_TARGET_TRIPLE"),
    );

    // Set executable location env vars
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var("FRANKENTERM_EXECUTABLE_DIR", dir);
                std::env::set_var("FRANKENTERM_EXECUTABLE", &exe);
                // WezTerm compat for vendored config crate
                std::env::set_var("WEZTERM_EXECUTABLE_DIR", dir);
                std::env::set_var("WEZTERM_EXECUTABLE", &exe);
            }
        }
    }

    // macOS: set LANG from locale if not already set
    #[cfg(target_os = "macos")]
    if std::env::var_os("LANG").map_or(true, |v| v.is_empty()) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("LANG", "en_US.UTF-8");
        }
    }

    // Clean up env vars that interfere with terminal operation
    #[allow(unused_unsafe)]
    unsafe {
        std::env::remove_var("WINDOWID");
        std::env::remove_var("VTE_VERSION");
        std::env::remove_var("SHELL");
    }
}

fn log_gui_macos_backend_selection() {
    let selection = probe_gui_macos_backend_selection();
    log::info!(
        "macOS renderer backend selection: backend={:?} reason={:?} override={:?} arch={:?} version={}.{}",
        selection.result.backend,
        selection.result.reason,
        selection.override_,
        selection.arch,
        selection.version.major,
        selection.version.minor
    );
}

fn probe_gui_macos_backend_selection() -> GuiMacosBackendSelection {
    let override_value = env::var(FT_MACOS_BACKEND_ENV).ok();
    select_gui_macos_backend(
        override_value.as_deref(),
        detect_gui_macos_arch(),
        detect_gui_macos_version(),
    )
}

fn select_gui_macos_backend(
    override_value: Option<&str>,
    arch: MacosArch,
    version: MacosVersion,
) -> GuiMacosBackendSelection {
    let override_ = override_value
        .map(BackendOverride::from_env_str)
        .unwrap_or_default();
    let result = select_macos_backend(BackendSelectionInputs::new(arch, version, override_));

    GuiMacosBackendSelection {
        override_,
        arch,
        version,
        result,
    }
}

fn detect_gui_macos_arch() -> MacosArch {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        MacosArch::AppleSilicon
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        MacosArch::IntelX64
    }
    #[cfg(not(target_os = "macos"))]
    {
        MacosArch::Unknown
    }
}

fn detect_gui_macos_version() -> MacosVersion {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    String::from_utf8(output.stdout).ok()
                } else {
                    None
                }
            })
            .and_then(|version| parse_macos_version(&version))
            .unwrap_or_default()
    }
    #[cfg(not(target_os = "macos"))]
    {
        MacosVersion::default()
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_version(version: &str) -> Option<MacosVersion> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some(MacosVersion::new(major, minor))
}

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    about = "FrankenTerm — Swarm-Native Terminal Emulator\nhttps://github.com/Dicklesworthstone/frankenterm",
    version = concat!("FrankenTerm ", env!("CARGO_PKG_VERSION"))
)]
struct Opt {
    /// Skip loading configuration file
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use, overrides the normal
    /// configuration file resolution
    #[arg(
        long = "config-file",
        value_parser,
        conflicts_with = "skip_config",
        value_hint=ValueHint::FilePath,
    )]
    config_file: Option<OsString>,

    /// Override specific configuration values
    #[arg(
        long = "config",
        name = "name=value",
        value_parser=ValueParser::new(name_equals_value),
        number_of_values = 1)]
    config_override: Vec<(String, String)>,

    /// On Windows, whether to attempt to attach to the parent
    /// process console to display logging output
    #[arg(long = "attach-parent-console")]
    #[allow(dead_code)]
    attach_parent_console: bool,

    #[command(subcommand)]
    cmd: Option<SubCommand>,
}

#[derive(Debug, Parser, Clone)]
enum SubCommand {
    #[command(
        name = "start",
        about = "Start the GUI, optionally running an alternative program [aliases: -e]"
    )]
    Start(StartCommand),

    /// Start the GUI in blocking mode. You shouldn't see this, but you
    /// may see it in shell completions because of this open clap issue:
    /// <https://github.com/clap-rs/clap/issues/1335>
    #[command(short_flag_alias = 'e', hide = true)]
    BlockingStart(StartCommand),

    #[command(name = "ssh", about = "Establish an ssh session")]
    Ssh(SshCommand),

    #[command(name = "serial", about = "Open a serial port")]
    Serial(SerialCommand),

    #[command(name = "connect", about = "Connect to FrankenTerm multiplexer")]
    Connect(ConnectCommand),

    #[command(name = "ls-fonts", about = "Display information about fonts")]
    LsFonts(LsFontsCommand),

    #[command(name = "show-keys", about = "Show key assignments")]
    ShowKeys(ShowKeysCommand),
}

async fn async_run_ssh(opts: SshCommand) -> anyhow::Result<()> {
    let mut ssh_option = HashMap::new();
    if opts.verbose {
        ssh_option.insert("wezterm_ssh_verbose".to_string(), "true".to_string());
    }
    for (k, v) in opts.config_override {
        ssh_option.insert(k.to_lowercase().to_string(), v);
    }

    let dom = SshDomain {
        name: format!("SSH to {}", opts.user_at_host_and_port),
        remote_address: opts.user_at_host_and_port.host_and_port.clone(),
        username: opts.user_at_host_and_port.username.clone(),
        multiplexing: SshMultiplexing::None,
        ssh_option,
        ..Default::default()
    };

    let mut start_command = StartCommand {
        always_new_process: true,
        class: opts.class,
        cwd: None,
        no_auto_connect: true,
        position: opts.position,
        workspace: None,
        prog: opts.prog.clone(),
        ..Default::default()
    };

    let cmd = if !opts.prog.is_empty() {
        let builder = CommandBuilder::from_argv(opts.prog);
        Some(builder)
    } else {
        None
    };

    let domain: Arc<dyn Domain> = Arc::new(mux::ssh::RemoteSshDomain::with_ssh_domain(&dom)?);
    let mux = Mux::try_get().context("mux singleton is not available")?;
    let domain_guard = mux.add_domain_and_acquire(&domain)?;
    start_command.domain = Some(domain_guard.domain_name().to_string());
    drop(domain);
    mux.set_default_domain_guard(&domain_guard)?;

    let should_publish = false;
    let result = async_run_terminal_gui(cmd, start_command, should_publish).await;
    drop(domain_guard);
    result
}

fn run_ssh(opts: SshCommand) -> anyhow::Result<()> {
    if let Some(cls) = opts.class.as_ref() {
        crate::set_window_class(cls);
    }
    if let Some(pos) = opts.position.as_ref() {
        set_window_position(pos.clone());
    }

    build_initial_mux(&config::configuration(), None, None)?;

    initialize_window_state_persistence();
    let gui = crate::frontend::try_new()?;
    let _mux_domain_config_subscription = subscribe_to_mux_domain_config_reload();

    let startup_reservation = match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        64 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => anyhow::bail!(
            "main-thread scheduler rejected mandatory SSH GUI startup before task construction: {rejected:?}"
        ),
    };
    startup_reservation
        .spawn_local(async {
            if let Err(err) = async_run_ssh(opts).await {
                terminate_with_error(err);
            }
        })
        .detach();

    maybe_show_configuration_error_window();
    run_gui_event_loop(gui)
}

async fn async_run_serial(opts: SerialCommand) -> anyhow::Result<()> {
    let serial_domain = SerialDomain {
        name: format!("Serial Port {}", opts.port),
        port: Some(opts.port.clone()),
        baud: opts.baud,
    };

    let mut start_command = StartCommand {
        always_new_process: true,
        class: opts.class,
        cwd: None,
        no_auto_connect: true,
        position: opts.position,
        workspace: None,
        domain: None,
        ..Default::default()
    };

    let cmd = None;

    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(serial_domain)?);
    let mux = Mux::try_get().context("mux singleton is not available")?;
    let domain_guard = mux.add_domain_and_acquire(&domain)?;
    start_command.domain = Some(domain_guard.domain_name().to_string());
    drop(domain);

    let should_publish = false;
    let result = async_run_terminal_gui(cmd, start_command, should_publish).await;
    drop(domain_guard);
    result
}

fn run_serial(config: config::ConfigHandle, opts: SerialCommand) -> anyhow::Result<()> {
    if let Some(cls) = opts.class.as_ref() {
        crate::set_window_class(cls);
    }
    if let Some(pos) = opts.position.as_ref() {
        set_window_position(pos.clone());
    }

    build_initial_mux(&config, None, None)?;

    initialize_window_state_persistence();
    let gui = crate::frontend::try_new()?;
    let _mux_domain_config_subscription = subscribe_to_mux_domain_config_reload();

    let startup_reservation = match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        64 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => anyhow::bail!(
            "main-thread scheduler rejected mandatory serial GUI startup before task construction: {rejected:?}"
        ),
    };
    startup_reservation
        .spawn_local(async {
            if let Err(err) = async_run_serial(opts).await {
                terminate_with_error(err);
            }
        })
        .detach();

    maybe_show_configuration_error_window();
    run_gui_event_loop(gui)
}

fn subscribe_to_mux_domain_config_reload() -> config::ConfigSubscription {
    config::subscribe_to_config_reload(move || {
        match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Topology,
            4 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                reservation
                    .spawn(async move {
                        // Fence the retired retry generation before changing
                        // the domain registry. Otherwise it can acquire and
                        // attach an old same-name registration while the
                        // replacement topology is being published.
                        cancel_auto_connect_supervisor();
                        if let Err(err) = update_mux_domains(&config::configuration()) {
                            let message = bounded_gui_failure_message(
                                "Failed to update mux domains after configuration reload",
                                &err,
                            );
                            frankenterm_gui::gui_debug_log::record(
                                log::Level::Error,
                                "frankenterm_gui::domain_config_reload",
                                message.clone(),
                            );
                            log::error!("{message}");
                            persistent_toast_notification(
                                "Domain configuration reload failed",
                                &message,
                            );
                            if AUTO_CONNECT_ENABLED.load(Ordering::Acquire) {
                                // Rebuild supervision from whichever registry
                                // state survived the failed update; leaving the
                                // old task cancelled forever would convert a
                                // transient reload failure into permanent loss
                                // of reconnect service.
                                schedule_auto_connect_domains();
                            }
                        } else if AUTO_CONNECT_ENABLED.load(Ordering::Acquire) {
                            schedule_auto_connect_domains();
                        }
                    })
                    .detach();
            }
            rejected => {
                metrics::counter!(
                    "gui.domain_config_reload_admission",
                    "outcome" => "terminal_rejection"
                )
                .increment(1);
                log::error!(
                    "main-thread scheduler rejected mux-domain config reload before task construction: {rejected:?}"
                );
            }
        }
        true
    })
}

fn have_panes_in_domain_and_ws(
    mux: &Mux,
    domain: &DomainOperationGuard,
    workspace: &Option<String>,
) -> bool {
    let window_ids = workspace.as_ref().map_or_else(
        || mux.iter_windows(),
        |ws| mux.iter_windows_in_workspace(ws),
    );

    window_ids
        .into_iter()
        .any(|window_id| mux.window_has_panes_in_domain(window_id, domain.domain_id()))
}

async fn populate_local_recovery_window_after_remote_failure(
    mux: &Arc<Mux>,
    failed_domain: &DomainOperationGuard,
    window_id: mux::window::WindowId,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let client = failed_domain
        .downcast_ref::<ClientDomain>()
        .context("local recovery policy requires a failed client domain")?;
    let message = domain_connection_failure_message(
        failed_domain.domain_name(),
        error,
        configured_remote_recovery(
            client.connect_automatically(),
            AUTO_CONNECT_ENABLED.load(Ordering::Acquire),
        ),
    );
    frankenterm_gui::gui_debug_log::record(
        log::Level::Error,
        "frankenterm_gui::remote_domain_recovery",
        message.clone(),
    );
    log::error!("{message}");
    persistent_toast_notification("Remote domain unavailable", &message);

    let recovery_domain = mux
        .get_domain_by_name("local")
        .context("local recovery domain is not available after remote attach failure")?;
    let config = config::configuration();
    let dpi = config.dpi.unwrap_or_else(::window::default_dpi);
    crate::spawn::attach_domain_to_window_or_spawn_recovery(
        &recovery_domain,
        window_id,
        None,
        None,
        dpi as u32,
    )
    .await
    .context("spawning local recovery shell after remote attach failure")?;
    trigger_and_log_gui_attached(MuxDomain(recovery_domain.domain_id())).await;
    Ok(())
}

async fn preserve_or_populate_window_after_remote_failure(
    mux: &Arc<Mux>,
    failed_domain: &DomainOperationGuard,
    window_id: mux::window::WindowId,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    if mux.window_has_panes_in_domain(window_id, failed_domain.domain_id()) {
        let message = domain_connection_failure_message(
            failed_domain.domain_name(),
            error,
            DomainConnectionRecovery::ExistingWindow,
        );
        frankenterm_gui::gui_debug_log::record(
            log::Level::Error,
            "frankenterm_gui::remote_domain_recovery",
            message.clone(),
        );
        log::error!("{message}");
        persistent_toast_notification("Remote domain partially opened", &message);
        trigger_and_log_gui_attached(MuxDomain(failed_domain.domain_id())).await;
        Ok(())
    } else {
        populate_local_recovery_window_after_remote_failure(
            mux,
            failed_domain,
            window_id,
            error,
        )
        .await
    }
}

async fn spawn_tab_in_domain_if_mux_is_empty(
    cmd: Option<CommandBuilder>,
    is_connecting: bool,
    domain: Option<DomainOperationGuard>,
    workspace: Option<String>,
) -> anyhow::Result<()> {
    let mux = Mux::try_get().context("mux singleton is not available")?;

    let domain = match domain {
        Some(domain) => domain,
        None => mux
            .default_domain()
            .context("resolving the default mux domain for initial tab spawn")?,
    };

    if !is_connecting && have_panes_in_domain_and_ws(&mux, &domain, &workspace) {
        return Ok(());
    }

    let window_id = {
        // Force the builder to notify the frontend early,
        // so that the attach await below doesn't block it.
        // This has the consequence of creating the window
        // at the initial size instead of populating it
        // from the size specified in the remote mux.
        // We use the frozen WindowTopologyChanged attachment payload
        // to detect and adjust the size later on.
        let position = None;
        let builder = mux.new_empty_window(workspace.clone(), position);
        *builder
    };

    let config = config::configuration();
    let dpi = config.dpi.unwrap_or_else(::window::default_dpi);
    if let Err(error) = crate::spawn::attach_domain_to_window_or_spawn_recovery(
        &domain, window_id, cmd, None, dpi as u32,
    )
    .await
    {
        if domain.downcast_ref::<ClientDomain>().is_some() {
            preserve_or_populate_window_after_remote_failure(
                &mux, &domain, window_id, &error,
            )
            .await?;
            return Ok(());
        }
        return Err(error);
    }
    trigger_and_log_gui_attached(MuxDomain(domain.domain_id())).await;
    Ok(())
}

async fn attempt_independent_auto_connects<I, A, F>(
    domain_names: I,
    mut attach: A,
) -> Vec<(String, anyhow::Error)>
where
    I: IntoIterator<Item = String>,
    A: FnMut(String) -> F,
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    use futures::StreamExt as _;

    const MAX_CONCURRENT_AUTO_CONNECTS: usize = 4;

    let mut failures = Vec::new();
    let mut attempts = futures::stream::iter(domain_names)
        .map(move |name| {
            let future = attach(name.clone());
            async move { (name, future.await) }
        })
        .buffer_unordered(MAX_CONCURRENT_AUTO_CONNECTS);
    while let Some((name, result)) = attempts.next().await {
        if let Err(error) = result {
            failures.push((name, error));
        }
    }
    failures
}

fn configured_auto_connect_domain_names(mux: &Arc<Mux>) -> Vec<String> {
    mux.iter_domains()
        .into_iter()
        .filter_map(|domain| {
            domain
                .downcast_ref::<ClientDomain>()
                .filter(|client| client.connect_automatically())
                .map(|_| domain.domain_name().to_string())
        })
        .collect()
}

fn auto_connect_domain_names(
    mux: &Arc<Mux>,
) -> Result<Vec<String>, domain_reconnect_manifest::DomainReconnectManifestError> {
    let manifest = domain_reconnect_manifest::load()?;
    Ok(mux
        .iter_domains()
        .into_iter()
        .filter_map(|domain| {
            domain.downcast_ref::<ClientDomain>().and_then(|client| {
                manifest
                    .should_connect(domain.domain_name(), client.connect_automatically())
                    .then(|| domain.domain_name().to_string())
            })
        })
        .collect())
}

async fn attempt_auto_connect_round(
    mux: &Arc<Mux>,
    domain_names: Vec<String>,
) -> Vec<(String, anyhow::Error)> {
    attempt_independent_auto_connects(domain_names, {
        let mux = Arc::clone(mux);
        move |domain_name| {
            let mux = Arc::clone(&mux);
            async move {
                let Some(domain) = mux.get_domain_by_name(&domain_name) else {
                    // A config reload retired this name. The newer supervisor
                    // generation owns whatever replaced it, so this attempt is
                    // complete rather than retryable.
                    return Ok(());
                };
                if domain.downcast_ref::<ClientDomain>().is_none() {
                    return Ok(());
                }
                let owner_client_id = mux.active_identity();
                domain.attach(&mux, owner_client_id, None).await
            }
        }
    })
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DomainConnectionRecovery {
    AutomaticRetry,
    LocalRecoveryShell,
    ExistingWindow,
    NoRecovery,
}

fn configured_remote_recovery(
    connect_automatically: bool,
    supervisor_enabled: bool,
) -> DomainConnectionRecovery {
    if connect_automatically && supervisor_enabled {
        DomainConnectionRecovery::AutomaticRetry
    } else {
        DomainConnectionRecovery::LocalRecoveryShell
    }
}

struct BoundedErrorWriter {
    text: String,
    remaining: usize,
}

impl std::fmt::Write for BoundedErrorWriter {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.remaining == 0 {
            return Err(std::fmt::Error);
        }
        let mut retained_len = value.len().min(self.remaining);
        while !value.is_char_boundary(retained_len) {
            retained_len -= 1;
        }
        self.text.push_str(&value[..retained_len]);
        self.remaining -= retained_len;
        if retained_len == value.len() {
            Ok(())
        } else {
            Err(std::fmt::Error)
        }
    }
}

fn bounded_gui_error_detail(error: &anyhow::Error) -> String {
    use frankenterm_core::output::sanitize_redact_truncate_bounded;
    use frankenterm_core::policy::Redactor;
    use std::fmt::Write as _;

    let mut rendered_error = BoundedErrorWriter {
        text: String::with_capacity(4_096),
        remaining: 4_096,
    };
    let _ = write!(&mut rendered_error, "{error:#}");
    let redactor = Redactor::new();
    sanitize_redact_truncate_bounded(&rendered_error.text, 320, 1_152, |text| {
        redactor.redact(text)
    })
}

fn bounded_gui_failure_message(summary: &str, error: &anyhow::Error) -> String {
    use frankenterm_core::output::sanitize_redact_truncate_bounded;
    use frankenterm_core::policy::Redactor;

    let redactor = Redactor::new();
    let safe_summary = sanitize_redact_truncate_bounded(summary, 128, 384, |text| {
        redactor.redact(text)
    });
    format!("{safe_summary}: {}", bounded_gui_error_detail(error))
}

fn domain_connection_failure_message(
    domain_name: &str,
    error: &anyhow::Error,
    recovery: DomainConnectionRecovery,
) -> String {
    use frankenterm_core::output::sanitize_redact_truncate_bounded;
    use frankenterm_core::policy::Redactor;
    let redactor = Redactor::new();
    let safe_domain = sanitize_redact_truncate_bounded(domain_name, 96, 256, |text| {
        redactor.redact(text)
    });
    let safe_error = bounded_gui_error_detail(error);
    let recovery = match recovery {
        DomainConnectionRecovery::AutomaticRetry => {
            "GUI startup will continue and the domain will retry automatically after its remote mux is available"
        }
        DomainConnectionRecovery::LocalRecoveryShell => {
            "GUI startup will continue in a local recovery shell; retry the domain after its remote mux is available"
        }
        DomainConnectionRecovery::ExistingWindow => {
            "the current window remains usable; retry the domain after its remote mux is available"
        }
        DomainConnectionRecovery::NoRecovery => {
            "the requested domain could not be opened"
        }
    };
    format!("connection to domain `{safe_domain}` failed; {recovery}: {safe_error}")
}

async fn supervise_auto_connect_domains(
    mux: Arc<Mux>,
    generation: u64,
    mut pending: Vec<String>,
) {
    let mut round = 0_u64;
    let mut retry_delay = std::time::Duration::from_secs(1);
    const MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

    while !pending.is_empty()
        && AUTO_CONNECT_ENABLED.load(Ordering::Acquire)
        && AUTO_CONNECT_SUPERVISOR_GENERATION.load(Ordering::Acquire) == generation
    {
        let Some(next_round) = round.checked_add(1) else {
            let message = "automatic domain connection retry counter exhausted; refusing to wrap retry identity";
            frankenterm_gui::gui_debug_log::record(
                log::Level::Error,
                "frankenterm_gui::auto_connect",
                message,
            );
            log::error!("{message}");
            persistent_toast_notification("Domain auto-connect unavailable", message);
            return;
        };
        round = next_round;
        let failures = attempt_auto_connect_round(&mux, pending).await;
        if AUTO_CONNECT_SUPERVISOR_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        if failures.is_empty() {
            return;
        }

        pending = Vec::with_capacity(failures.len());
        let mut first_round_toast = None;
        for (domain_name, error) in failures {
            pending.push(domain_name.clone());
            if round == 1 || round % 20 == 0 {
                // Auto-connect is independent per exact configured domain. A
                // single unavailable or mixed-version remote used to abort the
                // loop and surrounding GUI startup, so later domains were not
                // attempted and no normal window was published. Persist the
                // first failure and sparse reminders without creating an
                // unbounded high-frequency diagnostic stream.
                let message = domain_connection_failure_message(
                    &domain_name,
                    &error,
                    DomainConnectionRecovery::AutomaticRetry,
                );
                frankenterm_gui::gui_debug_log::record(
                    log::Level::Error,
                    "frankenterm_gui::auto_connect",
                    message.clone(),
                );
                log::error!("{message}");
                if round == 1 && first_round_toast.is_none() {
                    first_round_toast = Some(message);
                }
            }
        }
        if let Some(message) = first_round_toast {
            persistent_toast_notification("Domain auto-connect failures", &message);
        }

        promise::spawn::sleep(auto_connect_retry_delay(
            retry_delay,
            generation,
            round,
        ))
        .await;
        retry_delay = retry_delay.saturating_mul(2).min(MAX_RETRY_DELAY);
    }
}

fn auto_connect_retry_delay(
    ceiling: std::time::Duration,
    generation: u64,
    round: u64,
) -> std::time::Duration {
    auto_connect_retry_delay_with_process_id(
        ceiling,
        generation,
        round,
        std::process::id(),
    )
}

fn auto_connect_retry_delay_with_process_id(
    ceiling: std::time::Duration,
    generation: u64,
    round: u64,
    process_id: u32,
) -> std::time::Duration {
    let ceiling_ms = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
    if ceiling_ms <= 1 {
        return ceiling;
    }
    let jitter_width = (ceiling_ms / 4).max(1);
    let mixed = generation
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(17)
        ^ round.wrapping_mul(0xbf58_476d_1ce4_e5b9).rotate_left(31)
        ^ u64::from(process_id).wrapping_mul(0x94d0_49bb_1331_11eb);
    let jitter = mixed % jitter_width.saturating_add(1);
    std::time::Duration::from_millis(ceiling_ms.saturating_sub(jitter))
}

fn cancel_auto_connect_supervisor() {
    let previous = AUTO_CONNECT_SUPERVISOR_TASK.with(|slot| slot.borrow_mut().take());
    // Drop only after releasing the RefCell borrow. Cancellation may dispose
    // future-owned domain guards and scheduler permits synchronously.
    drop(previous);
}

const fn auto_connect_supervisor_may_schedule(enabled: bool, startup_ready: bool) -> bool {
    enabled && startup_ready
}

fn schedule_auto_connect_domains() {
    if !auto_connect_supervisor_may_schedule(
        AUTO_CONNECT_ENABLED.load(Ordering::Acquire),
        AUTO_CONNECT_STARTUP_READY.load(Ordering::Acquire),
    ) {
        cancel_auto_connect_supervisor();
        return;
    }
    let Some(mux) = Mux::try_get() else {
        log::error!("cannot schedule domain auto-connect without the mux singleton");
        // A previously scheduled task owns an Arc to its mux generation. Do
        // not leave that retired topology retrying merely because the process
        // singleton disappeared before this replacement attempt.
        cancel_auto_connect_supervisor();
        return;
    };
    let pending = match auto_connect_domain_names(&mux) {
        Ok(pending) => pending,
        Err(error) => {
            // A damaged optional preference must never become connection
            // authority. Explicit configuration remains usable, while the
            // privacy-safe error tells the operator that remembered intent
            // was ignored rather than silently widening it.
            let message = format!(
                "remembered domain attachment intent is unavailable and was ignored; only explicit connect_automatically configuration will be used: {error}"
            );
            frankenterm_gui::gui_debug_log::record(
                log::Level::Error,
                "frankenterm_gui::auto_connect",
                message.clone(),
            );
            log::error!("{message}");
            persistent_toast_notification("Remembered domain connections unavailable", &message);
            configured_auto_connect_domain_names(&mux)
        }
    };
    if pending.is_empty() {
        cancel_auto_connect_supervisor();
        return;
    }
    match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Background,
        32 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
            let generation = match AUTO_CONNECT_SUPERVISOR_GENERATION.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |current| current.checked_add(1),
            ) {
                Ok(previous) => previous + 1,
                Err(_) => {
                    let message = "automatic domain connection generation exhausted; preserving the current supervisor rather than reviving an ambiguous retired generation";
                    frankenterm_gui::gui_debug_log::record(
                        log::Level::Error,
                        "frankenterm_gui::auto_connect",
                        message,
                    );
                    log::error!("{message}");
                    persistent_toast_notification("Domain auto-connect unavailable", message);
                    return;
                }
            };
            let task = reservation
                .spawn_local(supervise_auto_connect_domains(mux, generation, pending))
                .into_task();
            AUTO_CONNECT_SUPERVISOR_TASK.with(|slot| {
                let replaced = slot.borrow_mut().replace(task);
                // Replacement is intentional: the successor already owns a
                // fresh generation and scheduler permit, so only now is it
                // safe to cancel the prior supervisor.
                drop(replaced);
            });
        }
        rejected => {
            let message = format!(
                "main-thread scheduler rejected automatic domain connections before task construction: {rejected:?}; GUI startup will continue, any existing supervisor remains active, and a config reload or manual domain open can retry"
            );
            frankenterm_gui::gui_debug_log::record(
                log::Level::Error,
                "frankenterm_gui::auto_connect",
                message.clone(),
            );
            log::error!("{message}");
            persistent_toast_notification("Domain auto-connect unavailable", &message);
        }
    }
}

async fn trigger_gui_startup(
    lua: Option<Rc<mlua::Lua>>,
    spawn: Option<SpawnCommand>,
) -> anyhow::Result<bool> {
    let Some(lua) = lua else {
        return Ok(false);
    };

    let args = lua.pack_multi(spawn)?;
    config::lua::emit_event(lua.as_ref().clone(), ("gui-startup".to_string(), args)).await?;
    Ok(true)
}

async fn trigger_and_log_gui_startup(spawn_command: Option<SpawnCommand>) {
    let result =
        config::with_lua_config_on_main_thread(move |lua| trigger_gui_startup(lua, spawn_command))
            .await;
    log_gui_hook_result("gui-startup", result);
}

async fn trigger_gui_attached(
    lua: Option<Rc<mlua::Lua>>,
    domain: MuxDomain,
) -> anyhow::Result<bool> {
    let Some(lua) = lua else {
        return Ok(false);
    };

    let args = lua.pack_multi(domain)?;
    config::lua::emit_event(lua.as_ref().clone(), ("gui-attached".to_string(), args)).await?;
    Ok(true)
}

async fn trigger_and_log_gui_attached(domain: MuxDomain) {
    let result =
        config::with_lua_config_on_main_thread(move |lua| trigger_gui_attached(lua, domain)).await;
    log_gui_hook_result("gui-attached", result);
}

fn log_gui_hook_result(event_name: &str, result: anyhow::Result<bool>) {
    match result {
        Ok(true) => {
            let message = format!("{event_name} Lua event emitted");
            frankenterm_gui::gui_debug_log::record(
                log::Level::Info,
                "frankenterm_gui::lua",
                message.clone(),
            );
            log::debug!("{message}");
        }
        Ok(false) => {
            let message = format!("{event_name} Lua event unavailable: no Lua config is loaded");
            frankenterm_gui::gui_debug_log::record(
                log::Level::Warn,
                "frankenterm_gui::lua",
                message.clone(),
            );
            log::warn!("{message}");
        }
        Err(err) => {
            let message = bounded_gui_failure_message(
                &format!("while processing {event_name} event"),
                &err,
            );
            frankenterm_gui::gui_debug_log::record(
                log::Level::Error,
                "frankenterm_gui::lua",
                message.clone(),
            );
            log::error!("{message}");
            persistent_toast_notification("Error", &message);
        }
    }
}

fn register_gui_lua_modules() {
    let setup_funcs: [config::lua::SetupFunc; 5] = [
        termwiz_funcs::register,
        mux_lua::register,
        url_funcs::register,
        scripting::register,
        stats::register,
    ];

    for setup in setup_funcs {
        config::lua::add_context_setup_func(setup);
    }
}

fn cell_pixel_dims(config: &ConfigHandle, dpi: f64) -> anyhow::Result<(usize, usize)> {
    let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()), dpi as usize)?);
    let render_metrics = RenderMetrics::new(&fontconfig)?;
    Ok((
        render_metrics.cell_size.width as usize,
        render_metrics.cell_size.height as usize,
    ))
}

async fn async_run_terminal_gui(
    cmd: Option<CommandBuilder>,
    opts: StartCommand,
    should_publish: bool,
) -> anyhow::Result<()> {
    let pid = std::process::id();
    let unix_socket_path = config::RUNTIME_DIR.join(format!("frankenterm-gui-sock-{}", pid));
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("FRANKENTERM_UNIX_SOCKET", unix_socket_path.clone());
        std::env::set_var("WEZTERM_UNIX_SOCKET", unix_socket_path.clone());
    }
    wezterm_blob_leases::register_storage(Arc::new(
        wezterm_blob_leases::simple_tempdir::SimpleTempDir::new_in(&*config::CACHE_DIR)?,
    ))?;
    if let Err(err) = spawn_mux_server(unix_socket_path, should_publish) {
        log::warn!("{:#}", err);
    }

    let spawn_command = match &cmd {
        Some(cmd) => Some(SpawnCommand::from_command_builder(cmd)?),
        None => None,
    };

    // Apply the domain to the command
    let spawn_command = match (spawn_command, &opts.domain) {
        (Some(spawn), Some(name)) => Some(SpawnCommand {
            domain: SpawnTabDomain::DomainName(name.to_string()),
            ..spawn
        }),
        (None, Some(name)) => Some(SpawnCommand {
            domain: SpawnTabDomain::DomainName(name.to_string()),
            ..SpawnCommand::default()
        }),
        (spawn, None) => spawn,
    };
    let mux = Mux::try_get().context("mux singleton is not available")?;

    let domain = if let Some(name) = &opts.domain {
        let domain = mux
            .get_domain_by_name(name)
            .ok_or_else(|| anyhow!("invalid domain {name}"))?;
        Some(domain)
    } else {
        None
    };

    if !opts.attach {
        trigger_and_log_gui_startup(spawn_command).await;
    }

    let is_connecting = opts.attach;

    if let Some(domain) = &domain {
        if !opts.attach {
            let window_id = {
                // Force the builder to notify the frontend early,
                // so that the attach await below doesn't block it.
                let workspace = opts.workspace.clone();
                let position = None;
                let builder = mux.new_empty_window(workspace, position);
                *builder
            };

            let remote_open_result = async {
                if domain.downcast_ref::<ClientDomain>().is_some() {
                    let persisted_domain_name = domain.domain_name().to_string();
                    promise::spawn::spawn_into_new_thread(move || {
                        domain_reconnect_manifest::set_intent(
                            &persisted_domain_name,
                            DomainAttachmentIntent::Attached,
                        )
                        .map(|_| ())
                        .map_err(anyhow::Error::new)
                    })
                    .await
                    .context("persisting explicitly requested domain attachment intent")?;
                }
                let owner_client_id = mux.active_identity();
                domain
                    .attach(&mux, owner_client_id, Some(window_id))
                    .await?;
                let config = config::configuration();
                let dpi = config.dpi.unwrap_or_else(::window::default_dpi);
                let tab = domain
                    .spawn(
                        &mux,
                        config.initial_size(dpi as u32, Some(cell_pixel_dims(&config, dpi)?)),
                        cmd.clone(),
                        None,
                        window_id,
                    )
                    .await?;
                mux.activate_tab_exact_in_window(window_id, &tab, false)
                    .with_context(|| {
                        format!(
                            "domain `{}` spawned tab {}, but window {window_id} does not contain the exact registered tab",
                            domain.domain_name(),
                            tab.tab_id()
                        )
                    })?;
                Result::<(), anyhow::Error>::Ok(())
            }
            .await;

            if let Err(error) = remote_open_result {
                if domain.downcast_ref::<ClientDomain>().is_some() {
                    // An explicitly requested remote domain must not leave an
                    // inert empty window or terminate the whole GUI when its
                    // mux is temporarily unavailable or still on an older
                    // codec. Populate the already-published window with a
                    // local recovery shell; the independent auto-connect
                    // supervisor keeps retrying configured auto domains and
                    // will publish their recovered topology after success.
                    preserve_or_populate_window_after_remote_failure(
                        &mux, domain, window_id, &error,
                    )
                    .await?;
                    return Ok(());
                }
                return Err(anyhow!(domain_connection_failure_message(
                    domain.domain_name(),
                    &error,
                    DomainConnectionRecovery::NoRecovery,
                )));
            }
            trigger_and_log_gui_attached(MuxDomain(domain.domain_id())).await;
        }
    }
    spawn_tab_in_domain_if_mux_is_empty(cmd, is_connecting, domain, opts.workspace).await
}

#[derive(Debug)]
enum Publish {
    TryPath(PathBuf),
    NoConnect,
    NoConnectButShouldPublish,
}

impl Publish {
    pub fn resolve(
        mux: &Arc<Mux>,
        config: &ConfigHandle,
        always_new_process: bool,
    ) -> anyhow::Result<Self> {
        let default_domain = mux
            .default_domain()
            .context("resolving the default mux domain before GUI publication")?;
        if default_domain.domain_name() != config.default_domain.as_deref().unwrap_or("local") {
            return Ok(Self::NoConnect);
        }

        if always_new_process {
            return Ok(Self::NoConnect);
        }

        if config::is_config_overridden() {
            // They're using a specific config file: assume that it is
            // different from the running gui
            log::trace!("skip existing gui: config is different");
            return Ok(Self::NoConnect);
        }

        Ok(
            match frankenterm_client::discovery::resolve_gui_sock_path(
                &crate::termwindow::get_window_class(),
            ) {
                Ok(path) => Self::TryPath(path),
                Err(_) => Self::NoConnectButShouldPublish,
            },
        )
    }

    pub fn should_publish(&self) -> bool {
        match self {
            Self::TryPath(_) | Self::NoConnectButShouldPublish => true,
            Self::NoConnect => false,
        }
    }

    pub fn try_spawn(
        &mut self,
        cmd: Option<CommandBuilder>,
        config: &ConfigHandle,
        workspace: Option<&str>,
        domain: SpawnTabDomain,
        new_tab: bool,
    ) -> anyhow::Result<bool> {
        if let Publish::TryPath(gui_sock) = &self {
            let dom = config::UnixDomain {
                socket_path: Some(gui_sock.clone()),
                no_serve_automatically: true,
                ..Default::default()
            };
            let mut ui = mux::connui::ConnectionUI::new_headless();
            match frankenterm_client::client::Client::new_unix_domain(
                None,
                &dom,
                false,
                &mut ui,
                true,
                std::sync::Weak::new(),
            ) {
                Ok(client) => {
                    let executor = promise::spawn::ScopedExecutor::new();
                    let command = cmd.clone();
                    let res = block_on(executor.run(async move {
                        let vers = client.verify_version_compat(&ui).await?;

                        if vers.executable_path != std::env::current_exe().context("resolve executable path")? {
                            *self = Publish::NoConnect;
                            anyhow::bail!(
                                "Running GUI is a different executable from us, will start a new one");
                        }
                        if vers.config_file_path
                            != std::env::var_os("WEZTERM_CONFIG_FILE").map(Into::into)
                        {
                            *self = Publish::NoConnect;
                            anyhow::bail!(
                                "Running GUI has different config from us, will start a new one"
                            );
                        }

                        let window_id = if new_tab || config.prefer_to_spawn_tabs {
                            if let Ok(pane_id) = client.resolve_pane_id(None).await {
                                let panes = client.list_panes().await?;

                                let mut window_id = None;
                                'outer: for tabroot in panes.tabs {
                                    let mut cursor = tabroot.into_tree().cursor();

                                    loop {
                                        if let Some(entry) = cursor.leaf_mut() {
                                            if entry.pane_id == pane_id {
                                                window_id.replace(entry.window_id);
                                                break 'outer;
                                            }
                                        }
                                        match cursor.preorder_next() {
                                            Ok(c) => cursor = c,
                                            Err(_) => break,
                                        }
                                    }
                                }
                                window_id

                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        client
                            .spawn_v2(codec::SpawnV2 {
                                domain,
                                window_id,
                                command,
                                command_dir: None,
                                size: config.initial_size(0, None),
                                workspace: workspace.unwrap_or(
                                    config
                                        .default_workspace
                                        .as_deref()
                                        .unwrap_or(mux::DEFAULT_WORKSPACE)
                                ).to_string(),
                            })
                            .await
                    }));

                    match res {
                        Ok(res) => {
                            log::info!(
                                "Spawned your command via the existing GUI instance. \
                             Use frankenterm-gui start --always-new-process if you do not want this behavior. \
                             Result={:?}",
                                res
                            );
                            Ok(true)
                        }
                        Err(err) => {
                            log::trace!(
                                "while attempting to ask existing instance to spawn: {:#}",
                                err
                            );
                            Ok(false)
                        }
                    }
                }
                Err(err) => {
                    // Couldn't connect: it's probably a stale symlink.
                    // That's fine: we can continue with starting a fresh gui below.
                    log::trace!("{:#}", err);
                    Ok(false)
                }
            }
        } else {
            Ok(false)
        }
    }
}

fn spawn_mux_server(unix_socket_path: PathBuf, should_publish: bool) -> anyhow::Result<()> {
    let dispatch_config = frankenterm_mux_server_impl::dispatch::DispatchRuntimeConfig::production(
        frankenterm_mux_server_impl::dispatch::DispatchIoPreference::Auto,
    )
    .context("configure embedded production mux dispatch tracing")?;
    let mut listener = frankenterm_mux_server_impl::local::LocalListener::with_domain(
        &config::UnixDomain {
            socket_path: Some(unix_socket_path.clone()),
            ..Default::default()
        },
        dispatch_config,
    )?;
    std::thread::Builder::new()
        .name("ft-gui-mux-server".to_string())
        .spawn(move || {
            let name_holder;
            if should_publish {
                name_holder = frankenterm_client::discovery::publish_gui_sock_path(
                    &unix_socket_path,
                    &crate::termwindow::get_window_class(),
                );
                if let Err(err) = &name_holder {
                    log::warn!("{:#}", err);
                }
            }

            listener.run();
            std::fs::remove_file(unix_socket_path).ok();
        })
        .context("failed to spawn GUI mux server thread")?;

    Ok(())
}

fn setup_mux(
    local_domain: Arc<dyn Domain>,
    config: &ConfigHandle,
    default_domain_name: Option<&str>,
    default_workspace_name: Option<&str>,
) -> anyhow::Result<Arc<Mux>> {
    let mux = Arc::new(mux::Mux::new(Some(local_domain)));
    Mux::set_mux(&mux);
    let client_id = Arc::new(mux::client::ClientId::new());
    mux.register_client(client_id.clone());
    mux.replace_identity(Some(client_id));
    let default_workspace_name = default_workspace_name.unwrap_or(
        config
            .default_workspace
            .as_deref()
            .unwrap_or(mux::DEFAULT_WORKSPACE),
    );
    mux.set_active_workspace(default_workspace_name);
    crate::update::load_last_release_info_and_set_banner();
    update_mux_domains(config)?;

    let default_name =
        default_domain_name.unwrap_or(config.default_domain.as_deref().unwrap_or("local"));

    let domain = mux.get_domain_by_name(default_name).ok_or_else(|| {
        anyhow::anyhow!(
            "desired default domain '{}' was not found in mux!?",
            default_name
        )
    })?;
    mux.set_default_domain_guard(&domain)?;

    Ok(mux)
}

fn build_initial_mux(
    config: &ConfigHandle,
    default_domain_name: Option<&str>,
    default_workspace_name: Option<&str>,
) -> anyhow::Result<Arc<Mux>> {
    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
    setup_mux(domain, config, default_domain_name, default_workspace_name)
}

fn run_terminal_gui(opts: StartCommand, default_domain_name: Option<String>) -> anyhow::Result<()> {
    if let Some(cls) = opts.class.as_ref() {
        crate::set_window_class(cls);
    }
    if let Some(pos) = opts.position.as_ref() {
        set_window_position(pos.clone());
    }

    let config = config::configuration();
    let need_builder = !opts.prog.is_empty() || opts.cwd.is_some();

    let cmd = if need_builder {
        let prog = opts.prog.iter().map(|s| s.as_os_str()).collect::<Vec<_>>();
        let mut builder = config.build_prog(
            if prog.is_empty() { None } else { Some(prog) },
            config.default_prog.as_ref(),
            config.default_cwd.as_ref(),
        )?;
        if let Some(cwd) = &opts.cwd {
            builder.cwd(if cwd.is_relative() {
                current_dir()?.join(cwd).into_os_string().into()
            } else {
                Cow::Borrowed(cwd.as_ref())
            });
        }
        Some(builder)
    } else {
        None
    };

    let mux = build_initial_mux(
        &config,
        default_domain_name.as_deref(),
        opts.workspace.as_deref(),
    )?;

    // Start the authenticated native-event bridge on targets whose Unix socket
    // API exposes peer credentials. Other targets compile the GUI without a
    // bridge rather than pretending an unauthenticated transport is usable.
    #[cfg(all(
        unix,
        any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "dragonfly"
        )
    ))]
    let _native_bridge = {
        let ft_config = match frankenterm_core::config::Config::load() {
            Ok(config) => config,
            Err(error) => {
                log::warn!(
                    "Native event bridge disabled because ft configuration failed to load: {error}"
                );
                frankenterm_core::config::Config::default()
            }
        };
        let environment_override = std::env::var("WEZTERM_FT_SOCKET").ok();
        frankenterm_core::config::resolve_native_events_socket_path(
            ft_config.native.enabled,
            environment_override.as_deref(),
            &ft_config.native.socket_path,
        )
        .and_then(|socket_path| native_bridge::NativeEventBridge::start(&socket_path))
    };
    #[cfg(not(all(
        unix,
        any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "dragonfly"
        )
    )))]
    let _native_bridge = ();

    // First, let's see if we can ask an already running instance to do this.
    // We must do this before we start the gui frontend as the scheduler
    // requirements are different.
    let mut publish = Publish::resolve(
        &mux,
        &config,
        opts.always_new_process || opts.position.is_some(),
    )?;
    log::trace!("{:?}", publish);
    if publish.try_spawn(
        cmd.clone(),
        &config,
        opts.workspace.as_deref(),
        match &opts.domain {
            Some(name) => SpawnTabDomain::DomainName(name.to_string()),
            None => SpawnTabDomain::DefaultDomain,
        },
        opts.new_tab,
    )? {
        return Ok(());
    }

    initialize_window_state_persistence();

    let gui = crate::frontend::try_new()?;
    // Config reload is subscribed before the asynchronous startup transaction
    // settles. Keep reload callbacks from starting a retry generation against
    // an unpublished or ultimately failed initial topology.
    AUTO_CONNECT_STARTUP_READY.store(false, Ordering::Release);
    AUTO_CONNECT_ENABLED.store(!opts.no_auto_connect, Ordering::Release);
    let _mux_domain_config_subscription = subscribe_to_mux_domain_config_reload();
    let activity = Activity::new_for_mux(&mux);

    let startup_reservation = match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        64 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => anyhow::bail!(
            "main-thread scheduler rejected mandatory terminal GUI startup before task construction: {rejected:?}"
        ),
    };
    startup_reservation
        .spawn_local(async move {
            match async_run_terminal_gui(cmd, opts, publish.should_publish()).await {
                Ok(()) => {
                    AUTO_CONNECT_STARTUP_READY.store(true, Ordering::Release);
                    schedule_auto_connect_domains();
                }
                Err(err) => terminate_with_error(err),
            }
            drop(activity);
        })
        .detach();

    maybe_show_configuration_error_window();
    run_gui_event_loop(gui)
}

fn initialize_window_state_persistence() {
    // Pay the one-time persistence worker and validated restore-snapshot cost
    // before entering GUI callbacks. Every startup window then performs only a
    // bounded in-memory workspace lookup rather than rereading both journal
    // slots.
    if let Err(failure) = window_state_persist::initialize() {
        log::warn!(
            "window-state: could not initialize persistence coordinator ({:?})",
            failure.code()
        );
    }

    mux_lua::install_domain_intent_recorder(Arc::new(|domain_name, intent| {
        Box::pin(async move {
            if domain_name == "local" {
                return Ok(());
            }
            let intent = match intent {
                mux_lua::DomainIntent::Attached => DomainAttachmentIntent::Attached,
                mux_lua::DomainIntent::Detached => DomainAttachmentIntent::Detached,
            };
            promise::spawn::spawn_into_new_thread(move || {
                domain_reconnect_manifest::set_intent(&domain_name, intent)
                    .map(|_| ())
                    .map_err(anyhow::Error::new)
            })
            .await
        })
    }));
}

fn run_gui_event_loop(gui: Rc<crate::frontend::GuiFrontEnd>) -> anyhow::Result<()> {
    let result = gui.run_forever();
    flush_window_state_at_shutdown();
    result
}

fn flush_window_state_at_shutdown() {
    let receiver = match window_state_persist::flush() {
        Ok(receiver) => receiver,
        Err(failure) => {
            log::warn!(
                "window-state: could not request shutdown barrier ({:?})",
                failure.code()
            );
            return;
        }
    };
    match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(_)) => {}
        Ok(Err(failure)) => log::warn!(
            "window-state: shutdown barrier failed ({:?})",
            failure.code()
        ),
        Err(_) => log::warn!("window-state: shutdown barrier timed out"),
    }
}

fn fatal_toast_notification(title: &str, message: &str) {
    persistent_toast_notification(title, message);
    // We need a short delay otherwise the notification
    // will not show
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::new(2, 0));
}

fn notify_on_panic() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Audited catches mark the unwind before the hook runs. Suppress every
        // fatal surface here; catch_recoverable emits bounded telemetry only
        // after control safely reaches the recovery branch. This must precede
        // payload classification: plugin-controlled panic text can imitate an
        // EPIPE message but cannot override the audited marker.
        if frankenterm_sigpipe::is_recoverable_panic() {
            return;
        }
        // Unmarked EPIPE retains its deterministic quiet exit through the
        // inner shared hook. Never raise a toast for a closed pipeline.
        if frankenterm_sigpipe::panic_is_broken_pipe(info) {
            previous_hook(info);
            return;
        }

        let report_claim = frankenterm_sigpipe::FatalReportClaim::enter();
        if report_claim.is_owner() {
            fatal_toast_notification(
                "FrankenTerm fatal error",
                "FrankenTerm encountered an internal error and must exit.",
            );
        }
        // The claim stays live through delegation, so the shared sanitized
        // terminal hook cannot emit a duplicate report.
        previous_hook(info);
    }));
}

fn terminate_with_error_message(err: &str) -> ! {
    log::error!("{}; terminating", err);
    fatal_toast_notification("FrankenTerm Error", err);
    std::process::exit(1);
}

fn terminate_with_error(err: anyhow::Error) -> ! {
    let mut err_text = bounded_gui_failure_message("FrankenTerm startup failed", &err);

    let warnings = config::configuration_warnings_and_errors();
    if !warnings.is_empty() {
        use frankenterm_core::output::sanitize_redact_truncate_bounded;
        use frankenterm_core::policy::Redactor;

        let redactor = Redactor::new();
        let warning_text = warnings.join("\n");
        let safe_warnings = sanitize_redact_truncate_bounded(
            &warning_text,
            512,
            1_600,
            |text| redactor.redact(text),
        );
        err_text = format!("{err_text}\nConfiguration error: {safe_warnings}");
    }

    terminate_with_error_message(&err_text)
}

// The opt-in `glyphcache_unit` target compiles this production module graph
// under Rust's generated test harness. Excluding the application entry point at
// compile time prevents the harness from automatically starting the frontend;
// focused proof commands must still select tests whose own bodies do not call
// frontend/window constructors.
#[cfg(not(test))]
fn main() {
    // Install the privacy-bounded terminal hook before profiler or runtime
    // initialization. The GUI notifier wraps this chain below; reversing the
    // hook order would discard the notifier.
    frankenterm_sigpipe::exit_quietly_on_broken_pipe();

    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    // Retain the static build fence through LTO/strip so packaging can verify
    // identity without executing the GUI or creating a window.
    std::hint::black_box(FT_ATOMIC_COMPONENT_MARKER);

    config::designate_this_as_the_main_thread();
    config::assign_error_callback(mux::connui::show_configuration_error_message);
    notify_on_panic();
    log_renderer_rollout_env_overrides();
    if let Err(e) = run() {
        terminate_with_error(e);
    }
    Mux::shutdown();
    frontend::shutdown();
}

#[cfg(test)]
#[test]
fn internal_glyphcache_harness_excludes_application_entrypoint() {
    // Reaching this generated-harness test proves the mutually exclusive
    // `#[cfg(not(test))] fn main` above was not compiled into this executable.
    assert!(cfg!(test));
}

fn log_renderer_rollout_env_overrides() {
    let report = frankenterm_gui::rollout_env::resolve_canonical_renderer_rollouts_from_env(
        frankenterm_core::rollout_strategy::Marker::M0,
    );

    for override_ in report.overrides {
        log::info!(
            "renderer rollout env override: feature={} env={} value={:?} before={:?} requested={:?} validity={:?} after={:?}",
            override_.feature_id,
            override_.env_var,
            override_.raw_value,
            override_.before,
            override_.requested,
            override_.validity,
            override_.after,
        );
    }
}

fn maybe_show_configuration_error_window() {
    let warnings = config::configuration_warnings_and_errors();
    if !warnings.is_empty() {
        let err = warnings.join("\n");
        mux::connui::show_configuration_error_message(&err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::macos_backend_select::{BackendFallbackReason, MacosBackend};

    #[test]
    fn auto_connect_failure_does_not_skip_later_independent_domains() {
        let attempted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempted_by_attach = Arc::clone(&attempted);
        let failures = futures::executor::block_on(attempt_independent_auto_connects(
            ["domain-1", "domain-2", "domain-4"].map(str::to_string),
            move |domain_name| {
                let attempted = Arc::clone(&attempted_by_attach);
                async move {
                    attempted
                        .lock()
                        .expect("record auto-connect attempt")
                        .push(domain_name.clone());
                    if domain_name == "domain-1" {
                        anyhow::bail!("planted first-domain failure");
                    }
                    Ok(())
                }
            },
        ));

        let mut attempted = attempted
            .lock()
            .expect("read auto-connect attempts")
            .clone();
        attempted.sort();
        assert_eq!(
            attempted,
            vec!["domain-1", "domain-2", "domain-4"],
            "a failed first domain must not suppress later configured auto-connect domains"
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "domain-1");
        assert!(failures[0].1.to_string().contains("planted first-domain"));
    }

    #[test]
    fn auto_connect_constructs_only_the_bounded_active_frontier() {
        use std::future::Future as _;

        let constructed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let constructed_by_attach = Arc::clone(&constructed);
        let attempts = attempt_independent_auto_connects(
            (0..32).map(|index| format!("domain-{index}")),
            move |_domain_name| {
                constructed_by_attach.fetch_add(1, Ordering::AcqRel);
                futures::future::pending::<anyhow::Result<()>>()
            },
        );
        futures::pin_mut!(attempts);
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(matches!(
            attempts.as_mut().poll(&mut context),
            std::task::Poll::Pending
        ));
        let constructed = constructed.load(Ordering::Acquire);
        assert!(
            (1..=4).contains(&constructed),
            "only the bounded active frontier may construct connection futures, got {constructed}"
        );
    }

    #[test]
    fn failed_auto_connect_name_can_be_retried_without_replaying_successes() {
        let attempts = Arc::new(std::sync::Mutex::new(HashMap::<String, usize>::new()));
        let attempts_by_attach = Arc::clone(&attempts);
        let first_failures = futures::executor::block_on(attempt_independent_auto_connects(
            ["domain-1", "domain-2"].map(str::to_string),
            move |domain_name| {
                let attempts = Arc::clone(&attempts_by_attach);
                async move {
                    let mut attempts = attempts.lock().expect("record auto-connect attempt");
                    let count = attempts.entry(domain_name.clone()).or_default();
                    *count += 1;
                    if domain_name == "domain-1" && *count == 1 {
                        anyhow::bail!("planted transient incompatibility");
                    }
                    Ok(())
                }
            },
        ));
        assert_eq!(first_failures.len(), 1);
        let retry_names = first_failures
            .into_iter()
            .map(|(name, _error)| name)
            .collect::<Vec<_>>();

        let attempts_by_retry = Arc::clone(&attempts);
        let second_failures = futures::executor::block_on(attempt_independent_auto_connects(
            retry_names,
            move |domain_name| {
                let attempts = Arc::clone(&attempts_by_retry);
                async move {
                    *attempts
                        .lock()
                        .expect("record auto-connect retry")
                        .entry(domain_name)
                        .or_default() += 1;
                    Ok(())
                }
            },
        ));

        assert!(second_failures.is_empty());
        let attempts = attempts.lock().expect("read auto-connect attempt counts");
        assert_eq!(attempts.get("domain-1"), Some(&2));
        assert_eq!(
            attempts.get("domain-2"),
            Some(&1),
            "a successful domain must leave the retry set"
        );
    }

    #[test]
    fn auto_connect_retry_jitter_is_deterministic_bounded_and_nonzero() {
        let ceiling = std::time::Duration::from_secs(30);
        let first = auto_connect_retry_delay_with_process_id(ceiling, 7, 11, 101);
        let repeated = auto_connect_retry_delay_with_process_id(ceiling, 7, 11, 101);
        assert_eq!(first, repeated);
        assert!(first <= ceiling);
        assert!(first >= std::time::Duration::from_millis(22_500));
        assert_ne!(
            auto_connect_retry_delay_with_process_id(ceiling, 7, 11, 101),
            auto_connect_retry_delay_with_process_id(ceiling, 7, 11, 102),
            "different desktop processes must de-phase"
        );
        assert_eq!(
            auto_connect_retry_delay(std::time::Duration::from_millis(1), 1, 1),
            std::time::Duration::from_millis(1)
        );
    }

    #[test]
    fn auto_connect_failure_diagnostic_is_bounded_terminal_safe_and_redacted() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let domain = format!("prod\u{1b}]0;forged-title\u{7}{secret}");
        let oversized = format!("remote failure {secret} {}", "x".repeat(8_192));
        let error = anyhow!(oversized);

        let message = domain_connection_failure_message(
            &domain,
            &error,
            DomainConnectionRecovery::AutomaticRetry,
        );

        assert!(message.contains("connection to domain"));
        assert!(message.contains("[REDACTED]"));
        assert!(!message.contains(secret));
        assert!(!message.contains('\u{1b}'));
        assert!(!message.contains('\u{7}'));
        assert!(message.len() <= 1_600, "diagnostic exceeded its byte budget");

        let manual_message = domain_connection_failure_message(
            &domain,
            &error,
            DomainConnectionRecovery::LocalRecoveryShell,
        );
        assert!(manual_message.contains("local recovery shell"));
        assert!(!manual_message.contains("retry automatically"));
        assert!(!manual_message.contains(secret));
        assert!(!manual_message.contains('\u{1b}'));
        assert!(
            manual_message.len() <= 1_600,
            "manual diagnostic exceeded its byte budget"
        );

        let existing_window_message = domain_connection_failure_message(
            &domain,
            &error,
            DomainConnectionRecovery::ExistingWindow,
        );
        assert!(existing_window_message.contains("current window remains usable"));
        assert!(!existing_window_message.contains("local recovery shell"));
        assert!(!existing_window_message.contains(secret));
        assert!(!existing_window_message.contains('\u{1b}'));

        let generic_message = bounded_gui_failure_message("spawn\u{1b}]0;bad", &error);
        assert!(generic_message.contains("[REDACTED]"));
        assert!(!generic_message.contains(secret));
        assert!(!generic_message.contains('\u{1b}'));
        assert!(generic_message.len() <= 1_600);
    }

    #[test]
    fn disabled_auto_connect_never_promises_an_automatic_retry() {
        assert_eq!(
            configured_remote_recovery(true, false),
            DomainConnectionRecovery::LocalRecoveryShell
        );
        assert_eq!(
            configured_remote_recovery(true, true),
            DomainConnectionRecovery::AutomaticRetry
        );
        assert!(!auto_connect_supervisor_may_schedule(true, false));
        assert!(!auto_connect_supervisor_may_schedule(false, true));
        assert!(auto_connect_supervisor_may_schedule(true, true));
    }

    type GuardedStartupFuture = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), futures::channel::oneshot::Canceled>>>,
    >;

    fn poll_guarded_startup(
        future: &mut GuardedStartupFuture,
    ) -> std::task::Poll<Result<(), futures::channel::oneshot::Canceled>> {
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        future.as_mut().poll(&mut context)
    }

    fn park_startup_guard(
        domain_guard: DomainOperationGuard,
    ) -> (futures::channel::oneshot::Sender<()>, GuardedStartupFuture) {
        let (release_tx, release_rx) = futures::channel::oneshot::channel::<()>();
        let mut guarded_start: GuardedStartupFuture = Box::pin(async move {
            let result = release_rx.await;
            drop(domain_guard);
            result
        });
        assert!(
            matches!(
                poll_guarded_startup(&mut guarded_start),
                std::task::Poll::Pending
            ),
            "startup must park while retaining its exact domain guard"
        );
        (release_tx, guarded_start)
    }

    fn release_startup_guard(
        release_tx: futures::channel::oneshot::Sender<()>,
        mut guarded_start: GuardedStartupFuture,
    ) {
        release_tx.send(()).expect("release guarded startup await");
        assert!(matches!(
            poll_guarded_startup(&mut guarded_start),
            std::task::Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn ssh_exact_selector_fails_instead_of_retargeting_local_fallback() {
        let local: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("ssh-local-fallback").expect("create local fallback"));
        let local_id = local.domain_id();
        let mux = Arc::new(Mux::new(Some(local)));
        let local_guard = mux
            .get_domain(local_id)
            .expect("local fallback must remain exactly registered");

        let ssh: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("ssh-startup-race").expect("create guarded SSH domain"));
        let ssh_id = ssh.domain_id();
        let ssh_guard = mux
            .add_domain_and_acquire(&ssh)
            .expect("atomically publish and acquire SSH domain");
        let start_selector = ssh_guard.domain_name().to_string();
        mux.set_default_domain_guard(&ssh_guard)
            .expect("exact SSH guard should become default");
        assert!(
            mux.default_domain()
                .is_ok_and(|current| current.same_registration(&ssh_guard)),
            "SSH must be the exact default before the planted retirement"
        );

        let retirement_ssh = Arc::clone(&ssh);
        let same_id_successor = Arc::clone(&ssh);
        drop(ssh);
        let (release_guard_tx, guarded_start) = park_startup_guard(ssh_guard);
        let mux_for_retirement = Arc::clone(&mux);
        let (race_tx, race_rx) = std::sync::mpsc::sync_channel(0);
        let retirement = std::thread::spawn(move || {
            let retired = mux_for_retirement.domain_was_detached_if_same(&retirement_ssh);
            let same_id_rejected = matches!(
                mux_for_retirement.add_domain(&retirement_ssh),
                Err(mux::DomainRegistrationError::RetiredIdentifier { .. })
            );
            race_tx
                .send((retired, same_id_rejected))
                .expect("report deterministic SSH retirement race");
        });
        let (retired, same_id_rejected) = race_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("SSH retirement must reach its same-ID probe");
        assert!(retired, "racing retirement must close the exact SSH domain");
        assert!(
            same_id_rejected,
            "same-ID SSH publication must stay fenced while startup awaits"
        );
        assert!(
            mux.default_domain()
                .is_ok_and(|current| current.same_registration(&local_guard)),
            "retiring SSH may promote local only as the ambient default"
        );
        assert!(
            mux.get_domain_by_name(&start_selector).is_none(),
            "the explicit SSH selector must fail closed instead of resolving local"
        );
        assert_ne!(
            local_guard.domain_name(),
            start_selector,
            "the planted fallback must be observably different from SSH"
        );
        retirement
            .join()
            .expect("SSH retirement thread must not panic");

        release_startup_guard(release_guard_tx, guarded_start);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match mux.add_domain(&same_id_successor) {
                Ok(()) => break,
                Err(mux::DomainRegistrationError::RetiredIdentifier { .. }) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "SSH guard release did not release the same-ID fence"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("unexpected same-ID SSH publication failure: {error}"),
            }
        }
        assert!(
            mux.get_domain_by_name(&start_selector)
                .is_some_and(|current| {
                    current.domain_id() == ssh_id
                        && current.is_same_domain(&same_id_successor)
                        && !current.same_registration(&local_guard)
                }),
            "the explicit selector may resolve only the re-admitted SSH registration"
        );
    }

    #[test]
    fn serial_exact_name_rejects_distinct_id_retarget_until_guard_release() {
        let name = "serial-startup-race";
        let mux = Arc::new(Mux::new(None));
        let serial: Arc<dyn Domain> =
            Arc::new(LocalDomain::new(name).expect("create guarded serial domain"));
        let serial_id = serial.domain_id();
        let serial_guard = mux
            .add_domain_and_acquire(&serial)
            .expect("atomically publish and acquire serial domain");
        let start_selector = serial_guard.domain_name().to_string();
        let distinct_same_name: Arc<dyn Domain> =
            Arc::new(LocalDomain::new(name).expect("create distinct same-name domain"));
        let replacement_id = distinct_same_name.domain_id();
        assert_ne!(
            serial_id, replacement_id,
            "same-name negative control must use a distinct numeric ID"
        );
        assert!(!serial_guard.is_same_domain(&distinct_same_name));

        let retirement_serial = Arc::clone(&serial);
        let contender = Arc::clone(&distinct_same_name);
        drop(serial);
        let (release_guard_tx, guarded_start) = park_startup_guard(serial_guard);
        let mux_for_retirement = Arc::clone(&mux);
        let (race_tx, race_rx) = std::sync::mpsc::sync_channel(0);
        let retirement = std::thread::spawn(move || {
            let retired = mux_for_retirement.domain_was_detached_if_same(&retirement_serial);
            let same_name_rejected = matches!(
                mux_for_retirement.add_domain(&contender),
                Err(mux::DomainRegistrationError::NameInUse { .. })
            );
            race_tx
                .send((retired, same_name_rejected))
                .expect("report deterministic serial name-retirement race");
        });
        let (retired, same_name_rejected) = race_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("serial retirement must reach its same-name probe");
        assert!(
            retired,
            "racing retirement must close the exact serial domain"
        );
        assert!(
            same_name_rejected,
            "distinct-ID same-name publication must stay fenced while startup awaits"
        );
        assert!(
            mux.get_domain_by_name(&start_selector).is_none(),
            "serial name must fail closed instead of retargeting the contender"
        );
        retirement
            .join()
            .expect("serial retirement thread must not panic");

        release_startup_guard(release_guard_tx, guarded_start);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match mux.add_domain(&distinct_same_name) {
                Ok(()) => break,
                Err(mux::DomainRegistrationError::NameInUse { .. }) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "serial guard release did not release the exact-name fence"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("unexpected same-name publication failure: {error}"),
            }
        }
        assert!(
            mux.get_domain_by_name(&start_selector)
                .is_some_and(|current| {
                    current.domain_id() == replacement_id
                        && current.is_same_domain(&distinct_same_name)
                }),
            "serial name may retarget only after the old exact guard releases"
        );
    }

    #[test]
    fn exact_default_domain_setter_rejects_foreign_guard() {
        let foreign_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("foreign-default").expect("create foreign local domain"));
        let foreign_domain_id = foreign_domain.domain_id();
        let foreign_mux = Arc::new(Mux::new(Some(foreign_domain)));
        let foreign_guard = foreign_mux
            .get_domain(foreign_domain_id)
            .expect("foreign mux should admit its exact domain guard");

        let local_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("local-default").expect("create local domain"));
        let local_mux = Arc::new(Mux::new(Some(local_domain)));
        let original_default = local_mux
            .default_domain()
            .expect("local mux should retain its initial default");
        assert!(matches!(
            local_mux.set_default_domain_guard(&foreign_guard),
            Err(mux::DomainRegistrationError::DefaultNotRegistered { .. })
        ));
        assert!(
            local_mux
                .default_domain()
                .is_ok_and(|current| current.same_registration(&original_default)),
            "rejecting a foreign guard must not mutate the exact local default"
        );
    }

    #[test]
    fn ssh_default_domain_source_holds_atomic_guard_through_terminal_start() {
        let source = include_str!("main.rs");
        let start = source
            .find("async fn async_run_ssh(")
            .expect("SSH command implementation must remain present");
        let end = source[start..]
            .find("\nfn run_ssh(")
            .map(|offset| start + offset)
            .expect("SSH command implementation must remain bounded");
        let body = &source[start..end];
        let raw_setter = ["mux.set_default_", "domain(&domain)"].concat();
        let split_add = ["mux.add_", "domain(&domain)"].concat();
        assert!(
            !body.contains(&raw_setter),
            "SSH setup must not publish its creator Arc through the raw default setter"
        );
        assert!(
            !body.contains(&split_add),
            "SSH setup must not split domain publication from exact guard admission"
        );

        let atomic_add = ["mux.add_domain_", "and_acquire(&domain)"].concat();
        let atomic_add = body
            .find(&atomic_add)
            .expect("SSH setup must atomically publish and acquire its exact guard");
        let exact_name = body
            .find("start_command.domain = Some(domain_guard.domain_name().to_string())")
            .expect("SSH setup must derive its selector from the exact guard");
        let release_creator = body
            .find("drop(domain)")
            .expect("SSH setup must release its raw creator Arc explicitly");
        let guarded_setter = body
            .find("mux.set_default_domain_guard(&domain_guard)")
            .expect("SSH setup must publish only through the exact guard setter");
        let stored_await = [
            "let result = async_run_terminal_gui(",
            "cmd, start_command, should_publish).await;",
        ]
        .concat();
        let stored_await = body
            .find(&stored_await)
            .expect("SSH setup must store the terminal-start result while its guard is live");
        let release_guard = body
            .find("drop(domain_guard)")
            .expect("SSH setup must explicitly release its exact guard");
        assert!(
            atomic_add < exact_name
                && exact_name < release_creator
                && release_creator < guarded_setter
                && guarded_setter < stored_await
                && stored_await < release_guard,
            "SSH must retain atomic default-domain authority through terminal startup"
        );
    }

    #[test]
    fn serial_domain_source_holds_atomic_name_guard_through_terminal_start() {
        let source = include_str!("main.rs");
        let start = source
            .find("async fn async_run_serial(")
            .expect("serial command implementation must remain present");
        let end = source[start..]
            .find("\nfn run_serial(")
            .map(|offset| start + offset)
            .expect("serial command implementation must remain bounded");
        let body = &source[start..end];
        let split_add = ["mux.add_", "domain(&domain)"].concat();
        assert!(
            !body.contains(&split_add),
            "serial setup must not split domain publication from exact guard admission"
        );
        let atomic_add = ["mux.add_domain_", "and_acquire(&domain)"].concat();
        let atomic_add = body
            .find(&atomic_add)
            .expect("serial setup must atomically publish and acquire its exact guard");
        let exact_name = body
            .find("start_command.domain = Some(domain_guard.domain_name().to_string())")
            .expect("serial setup must derive its name selector from the exact guard");
        let release_creator = body
            .find("drop(domain)")
            .expect("serial setup must release its raw creator Arc explicitly");
        let stored_await = [
            "let result = async_run_terminal_gui(",
            "cmd, start_command, should_publish).await;",
        ]
        .concat();
        let stored_await = body
            .find(&stored_await)
            .expect("serial setup must store terminal-start result while its guard is live");
        let release_guard = body
            .find("drop(domain_guard)")
            .expect("serial setup must explicitly release its exact guard");
        assert!(
            atomic_add < exact_name
                && exact_name < release_creator
                && release_creator < stored_await
                && stored_await < release_guard,
            "serial must retain its atomic exact-name authority through terminal startup"
        );
    }

    #[test]
    fn setup_mux_moves_local_domain_without_retaining_creator_arc() {
        let source = include_str!("main.rs");
        let start = source
            .find("fn setup_mux(")
            .expect("mux setup implementation must remain present");
        let end = source[start..]
            .find("\nfn build_initial_mux(")
            .map(|offset| start + offset)
            .expect("mux setup implementation must remain bounded");
        let body = &source[start..end];
        let moved = ["Mux::new(Some(", "local_domain))"].concat();
        let cloned = ["local_domain.", "clone()"].concat();
        assert!(
            body.contains(&moved),
            "mux setup must transfer its sole local-domain creator Arc"
        );
        assert!(
            !body.contains(&cloned),
            "mux setup must not retain an extra local-domain creator Arc"
        );
    }

    #[test]
    fn gui_macos_backend_defaults_to_core_selector_auto_path() {
        let selection =
            select_gui_macos_backend(None, MacosArch::AppleSilicon, MacosVersion::new(14, 0));

        assert_eq!(selection.override_, BackendOverride::Auto);
        assert_eq!(selection.result.backend, MacosBackend::MetalDirect);
        assert_eq!(
            selection.result.reason,
            BackendFallbackReason::MetalDirectGranted
        );
    }

    #[test]
    fn gui_macos_backend_honors_wgpu_rollback_override() {
        let selection = select_gui_macos_backend(
            Some("wgpu"),
            MacosArch::AppleSilicon,
            MacosVersion::new(14, 0),
        );

        assert_eq!(selection.override_, BackendOverride::Wgpu);
        assert_eq!(selection.result.backend, MacosBackend::Wgpu);
        assert_eq!(
            selection.result.reason,
            BackendFallbackReason::OperatorOverrideWgpu
        );
    }

    #[test]
    fn gui_macos_backend_downgrades_forced_metal_on_unsupported_runtime() {
        let selection =
            select_gui_macos_backend(Some("metal"), MacosArch::IntelX64, MacosVersion::new(14, 0));

        assert_eq!(selection.override_, BackendOverride::MetalDirect);
        assert_eq!(selection.result.backend, MacosBackend::Wgpu);
        assert_eq!(
            selection.result.reason,
            BackendFallbackReason::OperatorOverrideDowngraded
        );
    }

    #[test]
    fn parse_macos_version_accepts_major_minor_patch() {
        assert_eq!(
            parse_macos_version("14.5.1"),
            Some(MacosVersion::new(14, 5))
        );
        assert_eq!(parse_macos_version("13"), Some(MacosVersion::new(13, 0)));
        assert_eq!(parse_macos_version("not-a-version"), None);
    }
}

/// GH #80: config-health propagation for informational CLI subcommands
/// (`ls-fonts`, `show-keys`).
///
/// Documented semantics:
/// - **Hard-broken config** (the load raised an error, so the process is
///   running on built-in defaults): every captured message is printed to
///   stderr and the subcommand exits nonzero. Reporting built-in defaults
///   as if they were the user's config is worse than failing.
/// - **Degraded config** (loaded, but some settings were discarded,
///   unknown, or deprecated): each warning is printed to stderr ahead of
///   normal stdout output, and the subcommand still exits 0 so a
///   merely-deprecated key does not break scripted use.
fn check_cli_config_health() -> anyhow::Result<()> {
    let error = config::configuration_result()
        .err()
        .map(|err| format!("{err}"));
    let messages = config::configuration_warnings_and_errors();
    let (lines, fatal) = render_config_health(error.as_deref(), &messages);
    for line in &lines {
        eprintln!("{line}");
    }
    if fatal {
        anyhow::bail!(
            "configuration failed to load (see stderr above); refusing to report built-in defaults as if they were your configuration"
        );
    }
    Ok(())
}

/// Pure rendering core for [`check_cli_config_health`], split out for unit
/// testing: turns the captured load error + combined warning/error messages
/// into labeled stderr lines and a fatality flag.
fn render_config_health(error: Option<&str>, messages: &[String]) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    for msg in messages {
        let label = if error == Some(msg.as_str()) {
            "config error"
        } else {
            "config warning"
        };
        lines.push(format!("{label}: {msg}"));
    }
    if let Some(err) = error {
        // `configuration_warnings_and_errors()` normally includes the error
        // as its first entry; keep the fatal line even if it did not.
        if !messages.iter().any(|msg| msg == err) {
            lines.push(format!("config error: {err}"));
        }
    }
    (lines, error.is_some())
}

#[cfg(test)]
mod config_health_tests {
    use super::render_config_health;

    /// GH #80 regression: a hard config error must be fatal and labeled.
    #[test]
    fn hard_error_is_fatal_and_labeled() {
        let messages = vec![
            "runtime error: PROBE_MARKER".to_string(),
            "Unknown field no_such_option_at_all".to_string(),
        ];
        let (lines, fatal) = render_config_health(Some("runtime error: PROBE_MARKER"), &messages);
        assert!(fatal);
        assert_eq!(lines[0], "config error: runtime error: PROBE_MARKER");
        assert_eq!(
            lines[1],
            "config warning: Unknown field no_such_option_at_all"
        );
    }

    /// GH #80 regression: warnings alone are loud on stderr but not fatal.
    #[test]
    fn warnings_only_are_loud_but_not_fatal() {
        let messages = vec!["Unknown field no_such_option_at_all".to_string()];
        let (lines, fatal) = render_config_health(None, &messages);
        assert!(!fatal);
        assert_eq!(
            lines,
            vec!["config warning: Unknown field no_such_option_at_all".to_string()]
        );
    }

    /// GH #80 regression: a healthy config produces no stderr chatter.
    #[test]
    fn healthy_config_is_silent() {
        let (lines, fatal) = render_config_health(None, &[]);
        assert!(!fatal);
        assert!(lines.is_empty());
    }

    /// The error stays fatal even if the combined message list somehow
    /// omitted it; the error line is synthesized so stderr always explains
    /// the nonzero exit.
    #[test]
    fn error_missing_from_messages_is_still_reported() {
        let (lines, fatal) = render_config_health(Some("boom"), &[]);
        assert!(fatal);
        assert_eq!(lines, vec!["config error: boom".to_string()]);
    }
}

fn run_show_keys(config: config::ConfigHandle, cmd: &ShowKeysCommand) -> anyhow::Result<()> {
    let map = crate::inputmap::InputMap::new(&config);
    if cmd.lua {
        map.dump_config(cmd.key_table.as_deref());
    } else {
        map.show_keys();
    }
    Ok(())
}

pub fn run_ls_fonts(config: config::ConfigHandle, cmd: &LsFontsCommand) -> anyhow::Result<()> {
    use frankenterm_font::parser::ParsedFont;

    // GH #80: config load errors are handled by check_cli_config_health()
    // before this subcommand runs — a broken config exits nonzero instead of
    // silently rendering built-in defaults.

    // Disable the normal config error UI window, as we don't have
    // a fully baked GUI environment running
    config::assign_error_callback(|err| eprintln!("{}", err));

    let font_config = Rc::new(frankenterm_font::FontConfiguration::new(
        Some(config.clone()),
        config.dpi.unwrap_or_else(::window::default_dpi) as usize,
    )?);

    let render_metrics = crate::utilsprites::RenderMetrics::new(&font_config)?;

    let bidi_hint = if config.bidi_enabled {
        Some(config.bidi_direction)
    } else {
        None
    };

    let unicode_version = config.unicode_version();

    let text = match (&cmd.text, &cmd.codepoints) {
        (Some(text), _) => Some(text.to_string()),
        (_, Some(codepoints)) => {
            let mut s = String::new();
            for cp in codepoints.split(",") {
                let cp = u32::from_str_radix(cp, 16)
                    .with_context(|| format!("{cp} is not a hex number"))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| anyhow!("{cp} is not a valid unicode codepoint value"))?;
                s.push(c);
            }
            Some(s)
        }
        _ => None,
    };

    if let Some(text) = &text {
        // Emulate the effect of output normalization
        let text = if config.normalize_output_to_unicode_nfc {
            text.nfc().collect()
        } else {
            text.to_string()
        };

        let line = Line::from_text(
            &text,
            &CellAttributes::default(),
            SEQ_ZERO,
            Some(&unicode_version),
        );
        let cell_clusters = line.cluster(bidi_hint);
        let ft_lib = frankenterm_font::ftwrap::Library::new()?;

        let mut glyph_cache = GlyphCache::new_in_memory(&font_config, 256)?;

        for cluster in cell_clusters {
            let style = font_config.match_style(&config, &cluster.attrs);
            let font = font_config.resolve_font(style)?;
            let presentation_width = PresentationWidth::with_cluster(&cluster);
            let infos = font
                .blocking_shape(
                    &cluster.text,
                    Some(cluster.presentation),
                    cluster.direction,
                    None,
                    Some(&presentation_width),
                )
                .unwrap();

            // We must grab the handles after shaping, so that we get the
            // revised list that includes system fallbacks!
            let handles = font.clone_handles();
            let faces: Vec<_> = handles
                .iter()
                .map(|p| ft_lib.face_from_locator(&p.handle).ok())
                .collect();

            let mut iter = infos.iter().peekable();

            let mut byte_lens = vec![];
            for c in cluster.text.chars() {
                let len = c.len_utf8();
                for _ in 0..len {
                    byte_lens.push(len);
                }
            }
            println!("{:?}", cluster.direction);

            while let Some(info) = iter.next() {
                let idx = cluster.byte_to_cell_idx(info.cluster as usize);
                let followed_by_space = match line.get_cell(idx + 1) {
                    Some(cell) => cell.str() == " ",
                    None => false,
                };

                let text = if cluster.direction == Direction::LeftToRight {
                    if let Some(next) = iter.peek() {
                        line.columns_as_str(idx..cluster.byte_to_cell_idx(next.cluster as usize))
                    } else {
                        let last_idx = cluster.byte_to_cell_idx(cluster.text.len() - 1);
                        line.columns_as_str(idx..last_idx + 1)
                    }
                } else {
                    let info_len = byte_lens[info.cluster as usize];
                    let last_idx = cluster.byte_to_cell_idx(info.cluster as usize + info_len - 1);
                    line.columns_as_str(idx..last_idx + 1)
                };

                let parsed = &handles[info.font_idx];
                let escaped = format!("{}", text.escape_unicode());
                let mut is_custom = false;

                let cached_glyph = glyph_cache.cached_glyph(
                    info,
                    style,
                    followed_by_space,
                    &font,
                    &render_metrics,
                    info.num_cells,
                )?;

                let mut texture = cached_glyph.texture.clone();

                if config.custom_block_glyphs {
                    if let Some(block) = info.only_char.and_then(BlockKey::from_char) {
                        texture.replace(glyph_cache.cached_block(block, &render_metrics)?);
                        println!(
                            "{:2} {:4} {:12} drawn by FrankenTerm because custom_block_glyphs=true: {:?}",
                            info.cluster, text, escaped, block
                        );
                        is_custom = true;
                    }
                }

                if !is_custom {
                    let glyph_name = faces[info.font_idx]
                        .as_ref()
                        .and_then(|face| {
                            face.get_glyph_name(info.glyph_pos)
                                .map(|name| format!("{},", name))
                        })
                        .unwrap_or_else(String::new);

                    println!(
                        "{:2} {:4} {:12} x_adv={:<2} cells={:<2} glyph={}{:<4} {}\n{:38}{}",
                        info.cluster,
                        text,
                        escaped,
                        cached_glyph.x_advance.get(),
                        info.num_cells,
                        glyph_name,
                        info.glyph_pos,
                        parsed.lua_name(),
                        "",
                        parsed.handle.diagnostic_string()
                    );
                }

                if cmd.rasterize_ascii {
                    let mut glyph = String::new();

                    if let Some(texture) = &cached_glyph.texture {
                        use ::window::bitmaps::ImageTexture;
                        if let Some(tex) = texture.texture.downcast_ref::<ImageTexture>() {
                            for y in texture.coords.min_y()..texture.coords.max_y() {
                                for &px in tex.image.borrow().horizontal_pixel_range(
                                    texture.coords.min_x() as usize,
                                    texture.coords.max_x() as usize,
                                    y as usize,
                                ) {
                                    let px = u32::from_be(px);
                                    let (b, g, r, a) = (
                                        (px >> 8) as u8,
                                        (px >> 16) as u8,
                                        (px >> 24) as u8,
                                        (px & 0xff) as u8,
                                    );
                                    // Use regular RGB for other terminals, but then
                                    // set RGBA for FrankenTerm
                                    glyph.push_str(&format!(
                                "\x1b[38:2::{r}:{g}:{b}m\x1b[38:6::{r}:{g}:{b}:{a}m\u{2588}\x1b[0m"
                            ));
                                }
                                glyph.push('\n');
                            }
                        }
                    }

                    if !is_custom {
                        println!(
                            "bearing: x={} y={}, offset: x={} y={}",
                            cached_glyph.bearing_x.get(),
                            cached_glyph.bearing_y.get(),
                            cached_glyph.x_offset.get(),
                            cached_glyph.y_offset.get(),
                        );
                    }
                    println!("{glyph}");
                }
            }
        }
        return Ok(());
    }

    println!("Primary font:");
    let default_font = font_config.default_font()?;
    println!(
        "{}",
        ParsedFont::lua_fallback(&default_font.clone_handles())
    );
    println!();

    for rule in &config.font_rules {
        println!();

        let mut condition = "When".to_string();
        if let Some(intensity) = &rule.intensity {
            condition.push_str(&format!(" Intensity={:?}", intensity));
        }
        if let Some(underline) = &rule.underline {
            condition.push_str(&format!(" Underline={:?}", underline));
        }
        if let Some(italic) = &rule.italic {
            condition.push_str(&format!(" Italic={:?}", italic));
        }
        if let Some(blink) = &rule.blink {
            condition.push_str(&format!(" Blink={:?}", blink));
        }
        if let Some(rev) = &rule.reverse {
            condition.push_str(&format!(" Reverse={:?}", rev));
        }
        if let Some(strikethrough) = &rule.strikethrough {
            condition.push_str(&format!(" Strikethrough={:?}", strikethrough));
        }
        if let Some(invisible) = &rule.invisible {
            condition.push_str(&format!(" Invisible={:?}", invisible));
        }

        println!("{}:", condition);
        let font = font_config.resolve_font(&rule.font)?;
        println!("{}", ParsedFont::lua_fallback(&font.clone_handles()));
        println!();
    }

    println!("Title font:");
    let title_font = font_config.title_font()?;
    println!("{}", ParsedFont::lua_fallback(&title_font.clone_handles()));
    println!();

    if cmd.list_system {
        let font_dirs = font_config.list_fonts_in_font_dirs();
        println!(
            "{} fonts found in your font_dirs + built-in fonts:",
            font_dirs.len()
        );
        for font in font_dirs {
            let pixel_sizes = if font.pixel_sizes.is_empty() {
                "".to_string()
            } else {
                format!(" pixel_sizes={:?}", font.pixel_sizes)
            };
            println!(
                "{} -- {}{}{}",
                font.lua_name(),
                font.aka(),
                font.handle.diagnostic_string(),
                pixel_sizes
            );
        }

        match font_config.list_system_fonts() {
            Ok(sys_fonts) => {
                println!(
                    "{} system fonts found using {:?}:",
                    sys_fonts.len(),
                    config.font_locator
                );
                for font in sys_fonts {
                    let pixel_sizes = if font.pixel_sizes.is_empty() {
                        "".to_string()
                    } else {
                        format!(" pixel_sizes={:?}", font.pixel_sizes)
                    };
                    println!(
                        "{} -- {}{}{}",
                        font.lua_name(),
                        font.aka(),
                        font.handle.diagnostic_string(),
                        pixel_sizes
                    );
                }
            }
            Err(err) => log::error!("Unable to list system fonts: {}", err),
        }
    }

    Ok(())
}

fn run() -> anyhow::Result<()> {
    // Inform the system of our AppUserModelID.
    // Without this, our toast notifications won't be correctly
    // attributed to our application.
    #[cfg(windows)]
    {
        unsafe {
            ::windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                ::windows::core::PCWSTR(wide_string("com.frankenterm.gui").as_ptr()),
            )
            .unwrap();
        }
    }

    let opts = Opt::parse();

    // This is a bit gross.
    // In order to not to automatically open a standard windows console when
    // we run, we use the windows_subsystem attribute at the top of this
    // source file.  That comes at the cost of causing the help output
    // to disappear if we are actually invoked from a console.
    // This AttachConsole call will attach us to the console of the parent
    // in that situation, but since we were launched as a windows subsystem
    // application we will be running asynchronously from the shell in
    // the command window, which means that it will appear to the user
    // that we hung at the end, when in reality the shell is waiting for
    // input but didn't know to re-draw the prompt.
    #[cfg(windows)]
    unsafe {
        if opts.attach_parent_console {
            winapi::um::wincon::AttachConsole(winapi::um::wincon::ATTACH_PARENT_PROCESS);
        }
    };

    // Inline bootstrap (env_bootstrap has too many Lua deps).
    // Sets version info, executable env vars, locale, cleans env.
    frankenterm_bootstrap();
    register_gui_lua_modules();

    stats::Stats::init()?;
    let _saver = umask::UmaskSaver::new();

    config::common_init(
        opts.config_file.as_ref(),
        &opts.config_override,
        opts.skip_config,
    )?;
    let config = config::configuration();
    if let Some(value) = &config.default_ssh_auth_sock {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("SSH_AUTH_SOCK", value);
        }
    }

    let sub = match opts.cmd.as_ref().cloned() {
        Some(SubCommand::BlockingStart(start)) => {
            // Act as if the normal start subcommand was used,
            // except that we always start a new instance.
            // This is needed for compatibility, because many tools assume
            // that "$TERMINAL -e $COMMAND" blocks until the command finished.
            SubCommand::Start(StartCommand {
                always_new_process: true,
                ..start
            })
        }
        Some(sub) => sub,
        None => {
            // Need to fake an argv0
            let mut argv = vec!["frankenterm-gui".to_string()];
            for a in &config.default_gui_startup_args {
                argv.push(a.clone());
            }
            SubCommand::try_parse_from(&argv).with_context(|| {
                format!(
                    "parsing the default_gui_startup_args config: {:?}",
                    config.default_gui_startup_args
                )
            })?
        }
    };

    match sub {
        SubCommand::Start(start) => {
            log::trace!("Using configuration: {:#?}\nopts: {:#?}", config, opts);
            let res = run_terminal_gui(start, None);
            wezterm_blob_leases::clear_storage();
            res
        }
        SubCommand::BlockingStart(_) => unreachable!(),
        SubCommand::Ssh(ssh) => run_ssh(ssh),
        SubCommand::Serial(serial) => run_serial(config, serial),
        SubCommand::Connect(connect) => run_terminal_gui(
            StartCommand {
                domain: Some(connect.domain_name.clone()),
                class: connect.class,
                workspace: connect.workspace,
                position: connect.position,
                prog: connect.prog,
                new_tab: connect.new_tab,
                always_new_process: true,
                attach: true,
                _cmd: false,
                no_auto_connect: false,
                cwd: None,
            },
            Some(connect.domain_name),
        ),
        SubCommand::LsFonts(cmd) => {
            check_cli_config_health()?;
            run_ls_fonts(config, &cmd)
        }
        SubCommand::ShowKeys(cmd) => {
            check_cli_config_health()?;
            run_show_keys(config, &cmd)
        }
    }
}
