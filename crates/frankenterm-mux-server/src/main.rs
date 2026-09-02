use anyhow::Context;
use clap::*;
use config::configuration;
#[cfg(feature = "jemalloc")]
use frankenterm_alloc as _;
use frankenterm_mux_server_impl::generation_lifetime::GenerationLifetimeLease;
use frankenterm_mux_server_impl::{
    MuxDomainUpdateOutcome, reconcile_mux_domains_for_server, update_mux_domains_for_server,
};
use mux::Mux;
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain};
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::{OsStr, OsString};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use wezterm_gui_subcommands::*;

/// [ft-gqbpk] Set by the SIGTERM / SIGINT handler registered in
/// `install_shutdown_signal_handlers`. The main executor loop polls
/// this flag between ticks and breaks out cleanly so `run()` returns
/// `Ok(())` and `main()` can invoke the existing
/// `wezterm_blob_leases::clear_storage()` shutdown path.
///
/// Previously the daemon entered `loop { executor.tick()? }` with no
/// signal handler installed, so SIGTERM triggered the default
/// "terminate immediately" action and skipped cleanup.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MuxDomainConfigAdmissionRetryState {
    Idle,
    Starting,
    Running,
}

static MUX_DOMAIN_CONFIG_ADMISSION_RETRY_STATE: std::sync::Mutex<
    MuxDomainConfigAdmissionRetryState,
> = std::sync::Mutex::new(MuxDomainConfigAdmissionRetryState::Idle);
const FT_ATOMIC_COMPONENT_MARKER: &str = env!("FT_ATOMIC_COMPONENT_MARKER");

/// [ft-gqbpk] Shared shutdown-flag handle. Tests use this to assert
/// the signal handler writes the expected state; production code
/// calls `shutdown_requested()` inside the executor loop.
#[must_use]
pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// [ft-gqbpk] Reset the shutdown flag. Exposed for tests that need
/// to exercise the polling loop without the signal handler firing
/// left-over state from a prior test.
#[cfg(test)]
pub(crate) fn reset_shutdown_flag_for_tests() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
}

/// [ft-gqbpk] Mark shutdown as requested directly. Used by the
/// test suite to simulate a signal without raising one.
#[cfg(test)]
pub(crate) fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// [ft-gqbpk] Signal-safe SIGTERM / SIGINT handler. Must only
/// perform async-signal-safe work — here, a single atomic
/// store — since it runs in signal context where almost all libc
/// functions are undefined behaviour.
#[cfg(unix)]
extern "C" fn shutdown_signal_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// [ft-gqbpk] Install SIGTERM + SIGINT handlers using `libc::signal`.
/// Kept minimal on purpose: no dependency on `signal-hook` or
/// Tokio signal handling, and no `sigaction` plumbing, because the
/// SimpleExecutor loop only needs a one-bit "someone asked us to
/// stop" signal and the handler body is async-signal-safe.
///
#[cfg(unix)]
#[allow(unsafe_code)]
fn install_shutdown_signal_handlers() {
    // SAFETY: This runs during single-threaded startup before worker threads
    // spawn. The handler is an `extern "C"` function with the POSIX signal
    // ABI and only performs an atomic store, which is async-signal-safe.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            shutdown_signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            shutdown_signal_handler as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(not(unix))]
fn install_shutdown_signal_handlers() {
    // Windows has no POSIX signals. The daemon surface is Unix-only
    // in practice (the `daemonize` path at daemonize.rs is
    // `#![cfg(unix)]`), so the non-Unix branch is a no-op.
}

#[allow(unsafe_code)]
fn set_process_env_for_mux_server_startup(name: &str, value: impl AsRef<OsStr>) {
    // SAFETY: This private wrapper is only used during mux-server startup before
    // worker threads spawn, or by tests that hold TEST_STATE. Those call sites
    // serialize process-wide environment mutation and avoid concurrent env
    // readers/writers.
    unsafe { std::env::set_var(name, value) };
}

#[allow(unsafe_code)]
fn remove_process_env_for_mux_server_startup(name: &str) {
    // SAFETY: This private wrapper is only used during mux-server startup before
    // worker threads spawn, or by tests that hold TEST_STATE. Those call sites
    // serialize process-wide environment mutation and avoid concurrent env
    // readers/writers.
    unsafe { std::env::remove_var(name) };
}

#[derive(Debug, Parser)]
#[command(
    about = "FrankenTerm headless mux server for remote fleets",
    version = env!("CARGO_PKG_VERSION"),
    trailing_var_arg = true,
)]
struct Opt {
    /// Skip loading wezterm.lua
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use, overrides the normal
    /// configuration file resolution
    #[arg(
        long,
        value_parser,
        conflicts_with = "skip_config",
        value_hint=ValueHint::FilePath,
    )]
    config_file: Option<OsString>,

    /// Override specific configuration values
    #[arg(
        long = "config",
        name = "name=value",
        value_parser = clap::builder::ValueParser::new(name_equals_value),
        number_of_values = 1)]
    config_override: Vec<(String, String)>,

    /// Detach from the foreground and become a background process
    #[arg(long = "daemonize", action = clap::ArgAction::Set, default_value_t = false)]
    daemonize: bool,

    /// Select the mux dispatch reactor backend. `auto` prefers the
    /// compiled io_uring wrapper on supported Linux kernels and
    /// otherwise falls back to the existing readiness-based backend.
    #[arg(long = "dispatch-io-backend", value_enum, default_value_t = DispatchIoBackendArg::Auto)]
    dispatch_io_backend: DispatchIoBackendArg,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long = "cwd", value_parser, value_hint=ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// Instead of executing your shell, run PROG.
    /// For example: `frankenterm-mux-server -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DispatchIoBackendArg {
    Auto,
    IoUring,
    Epoll,
    Kqueue,
    Poll,
}

impl From<DispatchIoBackendArg> for frankenterm_mux_server_impl::dispatch::DispatchIoPreference {
    fn from(value: DispatchIoBackendArg) -> Self {
        match value {
            DispatchIoBackendArg::Auto => Self::Auto,
            DispatchIoBackendArg::IoUring => Self::IoUring,
            DispatchIoBackendArg::Epoll => Self::Epoll,
            DispatchIoBackendArg::Kqueue => Self::Kqueue,
            DispatchIoBackendArg::Poll => Self::Poll,
        }
    }
}

fn main() {
    // GH#75: a downstream reader closing our piped stdout early must exit
    // 141 quietly without a fatal report. The hook remains required under the
    // shipped unwind profile because std's stdout macros still panic on EPIPE.
    frankenterm_sigpipe::exit_quietly_on_broken_pipe();

    // Retain the static build fence through LTO/strip.  Package verification
    // can therefore reject stale mux servers without starting one.
    std::hint::black_box(FT_ATOMIC_COMPONENT_MARKER);
    // Process-level ownership is intentional: listener threads and blob-lease
    // cleanup can remain active after `run` returns. A managed generation must
    // therefore stay pinned through cleanup, error reporting, and termination.
    let mut generation_lifetime = None;
    if let Err(err) = run(&mut generation_lifetime) {
        wezterm_blob_leases::clear_storage();
        log::error!("{:#}", err);
        std::process::exit(1);
    }
    wezterm_blob_leases::clear_storage();
    // Detached listener threads may still be settling. Never run the lease
    // destructor while this process exists; the kernel releases every pinned
    // descriptor and the shared flock atomically during process teardown.
    std::hint::black_box(&generation_lifetime);
    std::process::exit(0);
}

fn run(generation_lifetime: &mut Option<GenerationLifetimeLease>) -> anyhow::Result<()> {
    //stats::Stats::init()?;
    config::designate_this_as_the_main_thread();
    let _saver = umask::UmaskSaver::new();

    let opts = Opt::parse();

    // The headless server had no logger at all until 2026-09-02, so config
    // load errors and the bound socket paths were invisible (ft-xxfwy.35).
    // Default to `info` so the ready/socket lines print without any env;
    // `RUST_LOG` still overrides.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // The daemonizing parent never owns mux state. Every foreground process
    // and daemon re-exec child acquires before configuration or any other
    // fallible initialization, then transfers the guard into `main`'s scope.
    if !opts.daemonize {
        let lease = GenerationLifetimeLease::acquire_for_current_process()
            .context("acquire mux managed-generation lifetime authority")?;
        *generation_lifetime = Some(lease);
    }

    config::common_init(
        opts.config_file.as_ref(),
        &opts.config_override,
        opts.skip_config,
    )?;
    validate_explicit_config_file(opts.config_file.as_deref())?;
    match opts.config_file.as_deref() {
        Some(path) => log::info!(
            "frankenterm-mux-server-config source=explicit path={}",
            std::path::Path::new(path).display()
        ),
        None if opts.skip_config => log::info!("frankenterm-mux-server-config source=skip_config"),
        None => log::info!("frankenterm-mux-server-config source=default_search"),
    }

    let config = config::configuration();

    config.update_ulimit()?;
    if let Some(value) = &config.default_ssh_auth_sock {
        set_process_env_for_mux_server_startup("SSH_AUTH_SOCK", value.as_str());
    }

    if opts.daemonize {
        daemonize::spawn_daemonized_copy(daemonized_child_args(&opts), &config)?;
        return Ok(());
    }

    // The daemon re-exec child has `daemonize=false`, so it populated the slot
    // before initialization above. Log only content-free readiness metadata.
    if let Some(metadata) = generation_lifetime
        .as_ref()
        .and_then(GenerationLifetimeLease::metadata)
    {
        log::info!(
            "frankenterm-mux-server-generation-lifetime-ready generation={} generations_dev={} generations_ino={} generation_dev={} generation_ino={} lease_dev={} lease_ino={} executable_dev={} executable_ino={}",
            metadata.generation_id(),
            metadata.generations_directory().device(),
            metadata.generations_directory().inode(),
            metadata.generation_directory().device(),
            metadata.generation_directory().inode(),
            metadata.lifetime_lease().device(),
            metadata.lifetime_lease().inode(),
            metadata.executable().device(),
            metadata.executable().inode(),
        );
    } else {
        log::info!("frankenterm-mux-server-generation-lifetime-unmanaged");
    }

    // [ft-gqbpk] Install SIGTERM + SIGINT handlers before any startup path
    // that allocates persistent state or spawns listeners. Otherwise a signal
    // in the gap before the executor loop would still take the default
    // terminate-immediately path and skip `clear_storage()` cleanup.
    install_shutdown_signal_handlers();

    // Remove some environment variables that aren't super helpful or
    // that are potentially misleading when we're starting up the
    // server.
    // We may potentially want to look into starting/registering
    // a session of some kind here as well in the future.
    for name in &[
        "OLDPWD",
        "PWD",
        "SHLVL",
        "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET",
        "FRANKENTERM_UNIX_SOCKET",
        "_",
    ] {
        remove_process_env_for_mux_server_startup(name);
    }
    for name in &config::configuration().mux_env_remove {
        remove_process_env_for_mux_server_startup(name);
    }

    config::create_user_owned_dirs(config::CACHE_DIR.as_path())?;
    wezterm_blob_leases::register_storage(Arc::new(
        wezterm_blob_leases::simple_tempdir::SimpleTempDir::new_in(&*config::CACHE_DIR)?,
    ))?;

    let need_builder = !opts.prog.is_empty() || opts.cwd.is_some();

    let cmd = if need_builder {
        let mut builder = if opts.prog.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            CommandBuilder::from_argv(opts.prog)
        };
        if let Some(cwd) = opts.cwd {
            builder.cwd(cwd);
        }
        Some(builder)
    } else {
        None
    };

    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
    let mux = Arc::new(mux::Mux::new(Some(domain.clone())));
    Mux::set_mux(&mux);

    let executor = promise::spawn::SimpleExecutor::new();

    let dispatch_config = frankenterm_mux_server_impl::dispatch::DispatchRuntimeConfig::production(
        opts.dispatch_io_backend.into(),
    )
    .context("configure production mux dispatch tracing")?;

    spawn_listener(dispatch_config).map_err(|e| {
        log::error!("problem spawning listeners: {:?}", e);
        e
    })?;
    log::info!(
        "frankenterm-mux-server-ready unix_domains={} tls_servers={}",
        config.unix_domains.len(),
        config.tls_servers.len()
    );

    let activity = Activity::new_for_mux(&mux);

    let startup_reservation = match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        64 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => anyhow::bail!(
            "main-thread scheduler rejected mandatory mux-server startup before task construction: {rejected:?}"
        ),
    };
    startup_reservation
        .spawn_local(async move {
            if let Err(err) = async_run(cmd).await {
                terminate_with_error(err);
            }
            drop(activity);
        })
        .detach();

    // Retain the subscription for the full executor lifetime. Keeping it in
    // `async_run` dropped it as soon as startup completed, silently disabling
    // every later domain-config reload.
    let _mux_domain_config_subscription = subscribe_to_mux_domain_config_reload();

    while !shutdown_requested() {
        executor.tick()?;
    }

    // [ft-gqbpk] Graceful shutdown path. `run()` returns Ok(()) here
    // so `main()` runs `wezterm_blob_leases::clear_storage()` on the
    // success branch. Mux::shutdown() stops pending mux activity and
    // lets PTY/domain Drop implementations run.
    log::info!("frankenterm-mux-server: shutdown signal received, flushing pending state");
    Mux::shutdown();
    Ok(())
}

async fn trigger_mux_startup(lua: Option<Rc<mlua::Lua>>) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi(())?;
        config::lua::emit_event(lua.as_ref().clone(), ("mux-startup".to_string(), args)).await?;
    }
    Ok(())
}

fn subscribe_to_mux_domain_config_reload() -> config::ConfigSubscription {
    config::subscribe_to_config_reload(move || {
        // Config subscribers run while the configuration mutex is held. The
        // admitted task reads the new handle only after this callback returns.
        let generation = match mint_mux_domain_config_reconciliation_generation() {
            Some(generation) => generation,
            None => {
                metrics::counter!(
                    "mux.server.domain_config_reload_admission",
                    "outcome" => "generation_exhausted"
                )
                .increment(1);
                log::error!(
                    "mux-server domain-config reconciliation generation exhausted; refusing an ambiguous reload"
                );
                return true;
            }
        };

        match try_admit_mux_domain_config_reconciliation(generation) {
            MuxDomainConfigAdmission::Started => {}
            MuxDomainConfigAdmission::Retryable(rejection) => {
                metrics::counter!(
                    "mux.server.domain_config_reload_admission",
                    "outcome" => "retrying"
                )
                .increment(1);
                log::warn!(
                    "main-thread scheduler temporarily rejected mux-server domain-config reload; a single coordinator will retry the newest generation: {rejection}"
                );
                start_mux_domain_config_admission_retry();
            }
            MuxDomainConfigAdmission::Terminal(rejection) => {
                metrics::counter!(
                    "mux.server.domain_config_reload_admission",
                    "outcome" => "terminal_rejection"
                )
                .increment(1);
                log::error!(
                    "main-thread scheduler terminally rejected mux-server domain-config reload before task construction: {rejection}"
                );
            }
        }
        true
    })
}

fn mint_mux_domain_config_reconciliation_generation() -> Option<u64> {
    MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .ok()
        .and_then(|previous| previous.checked_add(1))
}

fn mux_domain_config_reconciliation_is_current(generation: u64) -> bool {
    MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.load(Ordering::Acquire) == generation
}

enum MuxDomainConfigAdmission {
    Started,
    Retryable(String),
    Terminal(String),
}

fn try_admit_mux_domain_config_reconciliation(generation: u64) -> MuxDomainConfigAdmission {
    use promise::spawn::MainThreadReservationOutcome;

    match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        16 * 1024,
    ) {
        MainThreadReservationOutcome::Reserved(reservation) => {
            reservation
                .spawn(reconcile_mux_domain_config_until_converged(generation))
                .detach();
            MuxDomainConfigAdmission::Started
        }
        rejected @ (MainThreadReservationOutcome::RetryableFull(_)
        | MainThreadReservationOutcome::RetiredGeneration(_)
        | MainThreadReservationOutcome::Coalesced(_)
        | MainThreadReservationOutcome::SchedulerUnavailable) => {
            MuxDomainConfigAdmission::Retryable(format!("{rejected:?}"))
        }
        rejected @ (MainThreadReservationOutcome::InvalidSize(_)
        | MainThreadReservationOutcome::AuthorityExhausted(_)) => {
            MuxDomainConfigAdmission::Terminal(format!("{rejected:?}"))
        }
    }
}

fn lock_mux_domain_config_admission_retry_state()
-> std::sync::MutexGuard<'static, MuxDomainConfigAdmissionRetryState> {
    MUX_DOMAIN_CONFIG_ADMISSION_RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| {
            log::error!(
                "mux-server domain-config admission retry state was poisoned; recovering serialized ownership"
            );
            poisoned.into_inner()
        })
}

fn ensure_mux_domain_config_admission_retry(
    start: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut state = lock_mux_domain_config_admission_retry_state();
    match *state {
        MuxDomainConfigAdmissionRetryState::Running => return Ok(()),
        MuxDomainConfigAdmissionRetryState::Starting => {
            // The startup owner holds this mutex through thread creation.
            // Observing STARTING after acquiring it therefore means poison
            // recovery exposed an abandoned handoff.
            log::error!("mux-server domain-config admission retry recovered an abandoned startup");
            *state = MuxDomainConfigAdmissionRetryState::Idle;
        }
        MuxDomainConfigAdmissionRetryState::Idle => {}
    }

    *state = MuxDomainConfigAdmissionRetryState::Starting;
    match start() {
        Ok(()) => {
            *state = MuxDomainConfigAdmissionRetryState::Running;
            Ok(())
        }
        Err(error) => {
            *state = MuxDomainConfigAdmissionRetryState::Idle;
            Err(error)
        }
    }
}

fn finish_mux_domain_config_admission_retry(observed_generation: u64) -> bool {
    let mut state = lock_mux_domain_config_admission_retry_state();
    let has_newer_request =
        MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.load(Ordering::Acquire) != observed_generation;
    *state = if has_newer_request {
        MuxDomainConfigAdmissionRetryState::Running
    } else {
        MuxDomainConfigAdmissionRetryState::Idle
    };
    has_newer_request
}

fn stop_mux_domain_config_admission_retry() {
    *lock_mux_domain_config_admission_retry_state() = MuxDomainConfigAdmissionRetryState::Idle;
}

fn start_mux_domain_config_admission_retry() {
    if let Err(error) = ensure_mux_domain_config_admission_retry(|| {
        thread::Builder::new()
            .name("ft-mux-server-domain-config-admission".to_string())
            .spawn(retry_mux_domain_config_admission)
            .map(|_thread| ())
    }) {
        log::error!(
            "failed to start mux-server domain-config admission retry coordinator: {error}"
        );
    }
}

fn retry_mux_domain_config_admission() {
    let mut delay = std::time::Duration::from_millis(10);
    let mut attempts = 0_u64;
    loop {
        if shutdown_requested() {
            stop_mux_domain_config_admission_retry();
            return;
        }
        let generation = MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.load(Ordering::Acquire);
        match try_admit_mux_domain_config_reconciliation(generation) {
            MuxDomainConfigAdmission::Started => {
                if MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.load(Ordering::Acquire) != generation
                {
                    continue;
                }
                if finish_mux_domain_config_admission_retry(generation) {
                    continue;
                }
                return;
            }
            MuxDomainConfigAdmission::Retryable(rejection) => {
                attempts = attempts.saturating_add(1);
                if attempts == 1 || attempts.is_multiple_of(100) {
                    log::warn!(
                        "mux-server domain-config reconciliation is waiting for main-thread admission (attempt {attempts}): {rejection}"
                    );
                }
                std::thread::sleep(delay);
                delay = delay
                    .saturating_mul(2)
                    .min(std::time::Duration::from_secs(1));
            }
            MuxDomainConfigAdmission::Terminal(rejection) => {
                log::error!(
                    "mux-server domain-config reconciliation admission became terminal: {rejection}"
                );
                if finish_mux_domain_config_admission_retry(generation) {
                    delay = std::time::Duration::from_millis(10);
                    attempts = 0;
                    continue;
                }
                return;
            }
        }
    }
}

async fn reconcile_mux_domain_config_until_converged(generation: u64) {
    if !mux_domain_config_reconciliation_is_current(generation) {
        return;
    }
    let config = config::configuration();
    if !mux_domain_config_reconciliation_is_current(generation) {
        return;
    }

    let mut retirement_round = 0_u64;
    let mut retry_delay = std::time::Duration::from_millis(25);
    const MAX_RECONCILIATION_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
    loop {
        if shutdown_requested() || !mux_domain_config_reconciliation_is_current(generation) {
            return;
        }
        match reconcile_mux_domains_for_server(&config) {
            Ok(MuxDomainUpdateOutcome::Converged) => {
                metrics::counter!(
                    "mux.server.domain_config_reload_reconciliation",
                    "outcome" => "converged"
                )
                .increment(1);
                return;
            }
            Ok(MuxDomainUpdateOutcome::PendingRetirements { domain_names }) => {
                retirement_round = retirement_round.saturating_add(1);
                if retirement_round == 1 || retirement_round.is_multiple_of(100) {
                    log::info!(
                        "mux-server domain-config reload is waiting for exact domain retirements before replacement: {domain_names:?}"
                    );
                }
                promise::spawn::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(MAX_RECONCILIATION_DELAY);
            }
            Err(error) => {
                metrics::counter!(
                    "mux.server.domain_config_reload_reconciliation",
                    "outcome" => "failed"
                )
                .increment(1);
                log::error!("Error reconciling mux-server domains: {error:#}");
                return;
            }
        }
    }
}

async fn async_run(cmd: Option<CommandBuilder>) -> anyhow::Result<()> {
    let mux = Mux::try_get().context("mux singleton is not available")?;
    let config = config::configuration();

    update_mux_domains_for_server(&config)?;
    let domain = mux.default_domain()?;

    {
        if let Err(err) = config::with_lua_config_on_main_thread(trigger_mux_startup).await {
            log::error!("while processing mux-startup event: {:#}", err);
        }
    }

    let have_panes_in_domain = mux
        .iter_panes()
        .iter()
        .any(|p| p.domain_id() == domain.domain_id());

    if !have_panes_in_domain {
        let workspace = None;
        let position = None;
        let window_id = mux.new_empty_window(workspace, position);
        let owner_client_id = mux.active_identity();
        domain
            .attach(&mux, owner_client_id, Some(*window_id))
            .await?;

        let _tab = domain
            .spawn(&mux, config.initial_size(0, None), cmd, None, *window_id)
            .await?;
    }
    Ok(())
}

fn terminate_with_error(err: anyhow::Error) -> ! {
    log::error!("{:#}; terminating", err);
    std::process::exit(1);
}

mod daemonize;
mod ossl;

fn set_mux_socket_environment(config: &config::ConfigHandle) {
    if let Some(unix_dom) = config.unix_domains.first() {
        let socket_path = unix_dom.socket_path();
        set_process_env_for_mux_server_startup("WEZTERM_UNIX_SOCKET", socket_path.as_os_str());
        set_process_env_for_mux_server_startup("FRANKENTERM_UNIX_SOCKET", socket_path.as_os_str());
    }
}

/// An explicit `--config-file` that does not load must stop the server.
///
/// `config::common_init` keeps the last good configuration (the defaults on a
/// fresh process) and only records the load error, which is the right
/// behaviour for the GUI's default search path but wrong for an operator who
/// named a file: the server would silently run with defaults and bind
/// `RUNTIME_DIR/sock` instead of the configured socket (ft-xxfwy.35).
fn validate_explicit_config_file(config_file: Option<&std::ffi::OsStr>) -> anyhow::Result<()> {
    let Some(path) = config_file else {
        return Ok(());
    };
    match config::configuration_result() {
        Ok(_) => Ok(()),
        Err(err) => Err(anyhow::anyhow!(
            "refusing to start: --config-file {} did not load: {err:#}",
            std::path::Path::new(path).display()
        )),
    }
}

fn daemonized_child_args(opts: &Opt) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--daemonize=false"),
        OsString::from("--dispatch-io-backend"),
        OsString::from(match opts.dispatch_io_backend {
            DispatchIoBackendArg::Auto => "auto",
            DispatchIoBackendArg::IoUring => "io-uring",
            DispatchIoBackendArg::Epoll => "epoll",
            DispatchIoBackendArg::Kqueue => "kqueue",
            DispatchIoBackendArg::Poll => "poll",
        }),
    ];
    if opts.skip_config {
        args.push(OsString::from("-n"));
    }
    if let Some(f) = &opts.config_file {
        args.push(OsString::from("--config-file"));
        args.push(f.clone());
    }
    for (name, value) in &opts.config_override {
        args.push(OsString::from("--config"));
        args.push(OsString::from(format!("{name}={value}")));
    }
    if let Some(cwd) = &opts.cwd {
        args.push(OsString::from("--cwd"));
        args.push(cwd.clone());
    }
    if !opts.prog.is_empty() {
        args.push(OsString::from("--"));
        args.extend(opts.prog.iter().cloned());
    }
    args
}

pub fn spawn_listener(
    dispatch_config: frankenterm_mux_server_impl::dispatch::DispatchRuntimeConfig,
) -> anyhow::Result<()> {
    let config = configuration();
    set_mux_socket_environment(&config);

    for unix_dom in &config.unix_domains {
        log::info!(
            "frankenterm-mux-server-listener domain={} socket={}",
            unix_dom.name,
            unix_dom.socket_path().display()
        );
        let mut listener = frankenterm_mux_server_impl::local::LocalListener::with_domain(
            unix_dom,
            dispatch_config.clone(),
        )?;
        thread::Builder::new()
            .name("local-mux-listener".to_string())
            .spawn(move || {
                listener.run();
            })
            .context("spawn local mux listener thread")?;
    }

    for tls_server in &config.tls_servers {
        ossl::spawn_tls_listener(tls_server, dispatch_config.clone())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, UnixDomain};
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct TestStateGuard<'a> {
        _lock: MutexGuard<'a, ()>,
    }

    impl Drop for TestStateGuard<'_> {
        fn drop(&mut self) {
            reset_test_state();
        }
    }

    fn lock_test_state() -> TestStateGuard<'static> {
        let lock = test_lock().lock().expect("lock");
        reset_test_state();
        TestStateGuard { _lock: lock }
    }

    fn make_opt() -> Opt {
        Opt {
            skip_config: false,
            config_file: None,
            config_override: Vec::new(),
            daemonize: false,
            dispatch_io_backend: DispatchIoBackendArg::Auto,
            cwd: None,
            prog: Vec::new(),
        }
    }

    fn make_config_with_unix_domains(domains: Vec<UnixDomain>) -> config::ConfigHandle {
        let mut config = Config::default_config();
        config.unix_domains = domains;
        config::use_this_configuration(config);
        config::configuration()
    }

    fn reset_test_state() {
        config::use_test_configuration();
        MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.store(0, Ordering::Release);
        stop_mux_domain_config_admission_retry();
        remove_process_env_for_mux_server_startup("WEZTERM_UNIX_SOCKET");
        remove_process_env_for_mux_server_startup("FRANKENTERM_UNIX_SOCKET");
    }

    /// An explicit `--config-file` that fails to load must stop the server
    /// instead of silently running on defaults (ft-xxfwy.35).
    #[test]
    fn explicit_config_file_that_does_not_load_fails_closed() {
        let _guard = lock_test_state();
        let path = std::env::temp_dir().join(format!(
            "frankenterm-mux-server-bad-config-{}.lua",
            std::process::id()
        ));
        std::fs::write(&path, "this is not lua {{{\n").expect("write fixture config");
        let as_os = path.clone().into_os_string();
        config::common_init(Some(&as_os), &[], false).expect("common_init records the load error");

        let err = validate_explicit_config_file(Some(path.as_os_str()))
            .expect_err("a broken explicit config must fail closed");
        let message = format!("{err:#}");
        assert!(
            message.contains(&path.display().to_string()),
            "error must name the file: {message}"
        );
        assert!(
            validate_explicit_config_file(None).is_ok(),
            "no explicit file means the default search path keeps its fallback semantics"
        );

        config::use_default_configuration();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn process_generation_lifetime_owner_spans_cleanup_and_exit() {
        let source = include_str!("main.rs");
        let main_start = source.find("fn main() {").expect("find main function");
        let run_start = source[main_start..]
            .find("\nfn run(")
            .map(|offset| main_start + offset)
            .expect("find run function");
        let main_source = &source[main_start..run_start];

        let owner = main_source
            .find("let mut generation_lifetime = None;")
            .expect("main owns lifetime guard slot");
        let run_call = main_source
            .find("run(&mut generation_lifetime)")
            .expect("main lends lifetime guard slot to run");
        let error_cleanup = main_source
            .find("wezterm_blob_leases::clear_storage();")
            .expect("error path clears blob storage");
        let error_log = main_source
            .find("log::error!")
            .expect("error path reports failure");
        let process_exit = main_source
            .find("std::process::exit(1);")
            .expect("error path exits process");
        let success_cleanup = main_source
            .rfind("wezterm_blob_leases::clear_storage();")
            .expect("success path clears blob storage");
        let success_retention = main_source
            .find("std::hint::black_box(&generation_lifetime);")
            .expect("success path visibly retains generation lifetime guard");
        let success_exit = main_source
            .find("std::process::exit(0);")
            .expect("success path exits without running the guard destructor");
        assert!(owner < run_call);
        assert!(run_call < error_cleanup);
        assert!(error_cleanup < error_log);
        assert!(error_log < process_exit);
        assert!(process_exit < success_cleanup);
        assert!(success_cleanup < success_retention);
        assert!(success_retention < success_exit);
        assert_ne!(error_cleanup, success_cleanup);

        let run_source = &source[run_start..];
        let parse = run_source
            .find("let opts = Opt::parse();")
            .expect("parse opts");
        let foreground = run_source
            .find("if !opts.daemonize {")
            .expect("separate mux owner from daemonizing parent");
        let acquire = run_source
            .find("GenerationLifetimeLease::acquire_for_current_process()")
            .expect("acquire generation lifetime guard");
        let store = run_source
            .find("*generation_lifetime = Some(lease);")
            .expect("transfer guard into main-owned slot");
        let common_init = run_source
            .find("config::common_init(")
            .expect("find fallible configuration initialization");
        assert!(parse < foreground);
        assert!(foreground < acquire);
        assert!(acquire < store);
        assert!(store < common_init);
    }

    #[test]
    fn mux_domain_config_generation_fences_stale_reconciliation() {
        let _guard = lock_test_state();

        let first = mint_mux_domain_config_reconciliation_generation()
            .expect("first reconciliation generation");
        assert!(mux_domain_config_reconciliation_is_current(first));

        let second = mint_mux_domain_config_reconciliation_generation()
            .expect("second reconciliation generation");
        assert!(!mux_domain_config_reconciliation_is_current(first));
        assert!(mux_domain_config_reconciliation_is_current(second));
    }

    #[test]
    fn mux_domain_config_generation_exhaustion_fails_closed() {
        let _guard = lock_test_state();

        MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.store(u64::MAX - 1, Ordering::Release);
        assert_eq!(
            mint_mux_domain_config_reconciliation_generation(),
            Some(u64::MAX)
        );
        assert_eq!(mint_mux_domain_config_reconciliation_generation(), None);
        assert_eq!(
            MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.load(Ordering::Acquire),
            u64::MAX,
            "generation exhaustion must not wrap stale authority back to zero"
        );
    }

    #[test]
    fn mux_domain_config_admission_retry_serializes_startup_and_handoff() {
        let _guard = lock_test_state();
        let starts = std::sync::atomic::AtomicUsize::new(0);

        let failed = ensure_mux_domain_config_admission_retry(|| {
            starts.fetch_add(1, Ordering::AcqRel);
            Err(std::io::Error::other("planted thread creation failure"))
        });
        assert!(failed.is_err());
        assert_eq!(
            *lock_mux_domain_config_admission_retry_state(),
            MuxDomainConfigAdmissionRetryState::Idle,
            "failed startup must not publish a retry handoff"
        );

        ensure_mux_domain_config_admission_retry(|| {
            starts.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
        .expect("publish successful retry owner");
        ensure_mux_domain_config_admission_retry(|| {
            starts.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
        .expect("coalesce behind running retry owner");
        assert_eq!(
            starts.load(Ordering::Acquire),
            2,
            "a running retry coordinator must retain sole startup ownership"
        );

        MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.store(7, Ordering::Release);
        assert!(!finish_mux_domain_config_admission_retry(7));
        assert_eq!(
            *lock_mux_domain_config_admission_retry_state(),
            MuxDomainConfigAdmissionRetryState::Idle
        );

        ensure_mux_domain_config_admission_retry(|| Ok(()))
            .expect("restart retry owner for newer-generation handoff");
        MUX_DOMAIN_CONFIG_RECONCILIATION_GENERATION.store(8, Ordering::Release);
        assert!(finish_mux_domain_config_admission_retry(7));
        assert_eq!(
            *lock_mux_domain_config_admission_retry_state(),
            MuxDomainConfigAdmissionRetryState::Running,
            "new request published before retirement must retain the existing retry owner"
        );
        stop_mux_domain_config_admission_retry();
    }

    #[test]
    fn jemalloc_feature_matches_allocator_backend() {
        #[cfg(feature = "jemalloc")]
        {
            assert!(frankenterm_alloc::jemalloc_enabled());
            assert_eq!(frankenterm_alloc::allocator_backend().as_str(), "jemalloc");
        }

        #[cfg(not(feature = "jemalloc"))]
        {
            assert_eq!(env!("CARGO_PKG_NAME"), "frankenterm-mux-server");
        }
    }

    #[test]
    fn set_mux_socket_environment_sets_both_socket_env_vars_from_first_domain() {
        let _guard = lock_test_state();

        let first_socket = PathBuf::from("/tmp/ft-test-first.sock");
        let second_socket = PathBuf::from("/tmp/ft-test-second.sock");
        let handle = make_config_with_unix_domains(vec![
            UnixDomain {
                name: "first".to_string(),
                socket_path: Some(first_socket.clone()),
                ..UnixDomain::default()
            },
            UnixDomain {
                name: "second".to_string(),
                socket_path: Some(second_socket),
                ..UnixDomain::default()
            },
        ]);

        set_mux_socket_environment(&handle);

        assert_eq!(
            std::env::var_os("WEZTERM_UNIX_SOCKET"),
            Some(first_socket.clone().into_os_string())
        );
        assert_eq!(
            std::env::var_os("FRANKENTERM_UNIX_SOCKET"),
            Some(first_socket.into_os_string())
        );
    }

    #[test]
    fn set_mux_socket_environment_leaves_existing_env_when_no_domains_exist() {
        let _guard = lock_test_state();

        let sentinel = PathBuf::from("/tmp/ft-existing.sock");
        set_process_env_for_mux_server_startup("WEZTERM_UNIX_SOCKET", sentinel.as_os_str());
        set_process_env_for_mux_server_startup("FRANKENTERM_UNIX_SOCKET", sentinel.as_os_str());

        let handle = make_config_with_unix_domains(Vec::new());
        set_mux_socket_environment(&handle);

        assert_eq!(
            std::env::var_os("WEZTERM_UNIX_SOCKET"),
            Some(sentinel.clone().into_os_string())
        );
        assert_eq!(
            std::env::var_os("FRANKENTERM_UNIX_SOCKET"),
            Some(sentinel.into_os_string())
        );
    }

    fn unsafe_allow_targets(source_name: &str, source: &str) -> Vec<String> {
        let lines: Vec<_> = source.lines().collect();
        let mut targets = Vec::new();

        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[allow(unsafe_code)]" {
                continue;
            }

            let target = lines[index + 1..]
                .iter()
                .map(|line| line.trim())
                .find(|line| {
                    !line.is_empty()
                        && !line.starts_with("#[")
                        && !line.starts_with("//")
                        && !line.starts_with("///")
                })
                .expect("allow(unsafe_code) must annotate a concrete item");

            let signature = target
                .split('{')
                .next()
                .unwrap_or(target)
                .trim()
                .to_string();
            targets.push(format!("{source_name}:{signature}"));
        }

        targets
    }

    #[test]
    fn unsafe_code_allowlist_stays_narrow() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains("unsafe_code = \"deny\""),
            "mux-server must deny unsafe by default"
        );
        assert!(
            !manifest.contains("unsafe_code = \"allow\""),
            "whole-crate unsafe allowance must not return"
        );

        let mut targets = Vec::new();
        targets.extend(unsafe_allow_targets("main.rs", include_str!("main.rs")));
        targets.extend(unsafe_allow_targets(
            "daemonize.rs",
            include_str!("daemonize.rs"),
        ));

        assert_eq!(
            targets,
            vec![
                "main.rs:fn install_shutdown_signal_handlers()",
                "main.rs:fn set_process_env_for_mux_server_startup(name: &str, value: impl AsRef<OsStr>)",
                "main.rs:fn remove_process_env_for_mux_server_startup(name: &str)",
                "daemonize.rs:fn fork() -> anyhow::Result<Fork>",
                "daemonize.rs:fn setsid() -> anyhow::Result<()>",
                "daemonize.rs:fn lock_pid_file(config: &config::ConfigHandle) -> anyhow::Result<std::fs::File>",
                "daemonize.rs:fn wait_for_intermediate_child(pid: pid_t) -> !",
                "daemonize.rs:fn current_pid() -> pid_t",
                "daemonize.rs:fn redirect_standard_streams(",
                "daemonize.rs:pub fn set_cloexec(fd: RawFd, enable: bool)",
            ]
        );
    }

    #[test]
    fn daemonized_child_args_forward_cli_state_and_prog_separator() {
        let mut opts = make_opt();
        opts.skip_config = true;
        opts.dispatch_io_backend = DispatchIoBackendArg::IoUring;
        opts.config_file = Some(OsString::from("/tmp/ft.toml"));
        opts.config_override = vec![
            ("mux.enabled".to_string(), "true".to_string()),
            ("tls.required".to_string(), "false".to_string()),
        ];
        opts.cwd = Some(OsString::from("/tmp/workspace"));
        opts.prog = vec![
            OsString::from("bash"),
            OsString::from("-lc"),
            OsString::from("pwd"),
        ];

        let args = daemonized_child_args(&opts);

        assert_eq!(
            args,
            vec![
                OsString::from("--daemonize=false"),
                OsString::from("--dispatch-io-backend"),
                OsString::from("io-uring"),
                OsString::from("-n"),
                OsString::from("--config-file"),
                OsString::from("/tmp/ft.toml"),
                OsString::from("--config"),
                OsString::from("mux.enabled=true"),
                OsString::from("--config"),
                OsString::from("tls.required=false"),
                OsString::from("--cwd"),
                OsString::from("/tmp/workspace"),
                OsString::from("--"),
                OsString::from("bash"),
                OsString::from("-lc"),
                OsString::from("pwd"),
            ]
        );
    }

    #[test]
    fn daemonized_child_args_omit_prog_separator_when_no_prog_is_present() {
        let opts = make_opt();
        let args = daemonized_child_args(&opts);

        assert_eq!(
            args,
            vec![
                OsString::from("--daemonize=false"),
                OsString::from("--dispatch-io-backend"),
                OsString::from("auto"),
            ]
        );
        assert!(
            !args.iter().any(|arg| arg == OsStr::new("--")),
            "separator should only appear when forwarding a child program"
        );
    }

    // ── ft-gqbpk SIGTERM/SIGINT graceful-shutdown regressions ────────

    /// Helper: atomically swap the shutdown flag to a known state
    /// around each test so a prior test firing the handler can't
    /// bleed into this one. All ft-gqbpk tests must serialize on
    /// `test_lock()` because the SHUTDOWN_REQUESTED flag is global.
    fn fresh_shutdown_state() -> MutexGuard<'static, ()> {
        let guard = test_lock().lock().expect("lock shutdown state");
        reset_shutdown_flag_for_tests();
        guard
    }

    #[test]
    fn shutdown_flag_starts_false() {
        let _g = fresh_shutdown_state();
        assert!(
            !shutdown_requested(),
            "fresh process state: shutdown flag must be false"
        );
    }

    #[test]
    fn request_shutdown_sets_flag() {
        let _g = fresh_shutdown_state();
        assert!(!shutdown_requested());
        request_shutdown();
        assert!(
            shutdown_requested(),
            "request_shutdown() must set the poll flag"
        );
    }

    #[test]
    fn reset_shutdown_flag_for_tests_clears_flag() {
        let _g = fresh_shutdown_state();
        request_shutdown();
        assert!(shutdown_requested());
        reset_shutdown_flag_for_tests();
        assert!(
            !shutdown_requested(),
            "reset helper must restore false state"
        );
    }

    /// Calls the raw signal-handler function directly — no actual
    /// signal is raised, so the test binary's own signal routing
    /// is not perturbed. Verifies the handler body does the
    /// minimum async-signal-safe thing: set the poll flag.
    #[cfg(unix)]
    #[test]
    fn shutdown_signal_handler_sets_flag_on_sigterm() {
        let _g = fresh_shutdown_state();
        assert!(!shutdown_requested());
        // Safety: the handler is async-signal-safe; calling it
        // directly from the test thread has no race-critical
        // invariants to preserve.
        shutdown_signal_handler(libc::SIGTERM);
        assert!(
            shutdown_requested(),
            "SIGTERM handler must flip the shutdown flag"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_signal_handler_sets_flag_on_sigint() {
        let _g = fresh_shutdown_state();
        assert!(!shutdown_requested());
        shutdown_signal_handler(libc::SIGINT);
        assert!(
            shutdown_requested(),
            "SIGINT handler must flip the shutdown flag"
        );
    }

    /// [ft-gqbpk] End-to-end poll-loop behavior: starting from a
    /// false flag, an empty poll loop runs; after `request_shutdown`,
    /// the same poll loop exits cleanly without error. Mirrors the
    /// production `while !shutdown_requested() { executor.tick()?; }`
    /// shape so a regression that breaks the polling contract would
    /// fail here.
    #[test]
    fn shutdown_poll_loop_exits_after_request() {
        let _g = fresh_shutdown_state();
        let mut ticks = 0u32;
        let max_ticks = 1_000u32;
        // Simulate 5 ticks before the signal arrives.
        let shutdown_after = 5u32;
        while !shutdown_requested() {
            ticks += 1;
            if ticks == shutdown_after {
                request_shutdown();
            }
            assert!(
                ticks <= max_ticks,
                "poll loop should exit long before {max_ticks} ticks"
            );
        }
        assert_eq!(
            ticks, shutdown_after,
            "poll loop must exit immediately once flag is set, not after one more tick"
        );
    }

    #[test]
    fn shutdown_poll_loop_skips_ticks_when_flag_already_set() {
        let _g = fresh_shutdown_state();
        request_shutdown();

        let mut ticks = 0u32;
        while !shutdown_requested() {
            ticks += 1;
        }

        assert_eq!(
            ticks, 0,
            "if shutdown is requested before loop entry, the poll loop must not tick at all"
        );
    }

    /// [ft-gqbpk] install_shutdown_signal_handlers must be
    /// idempotent — calling it twice must not corrupt state or
    /// leave dangling handler refs. Production uses single-call,
    /// but a defensive re-install (e.g. after a re-exec) should
    /// be safe.
    #[cfg(unix)]
    #[test]
    fn install_shutdown_signal_handlers_is_idempotent() {
        let _g = fresh_shutdown_state();
        install_shutdown_signal_handlers();
        install_shutdown_signal_handlers();
        // Direct handler invocation still works after multi-install.
        shutdown_signal_handler(libc::SIGTERM);
        assert!(shutdown_requested());
    }
}
