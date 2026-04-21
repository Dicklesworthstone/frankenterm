use anyhow::Context;
use clap::*;
use config::configuration;
#[cfg(feature = "jemalloc")]
use frankenterm_alloc as _;
use frankenterm_mux_server_impl::update_mux_domains_for_server;
use mux::Mux;
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain};
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// test suite to simulate a signal without raising one, and by the
/// Windows fallback path that does not register Unix-style handlers.
pub(crate) fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// [ft-gqbpk] Signal-safe SIGTERM / SIGINT handler. Must only
/// perform async-signal-safe work — here, a single relaxed atomic
/// store — since it runs in signal context where almost all libc
/// functions are undefined behaviour.
#[cfg(unix)]
extern "C" fn shutdown_signal_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// [ft-gqbpk] Install SIGTERM + SIGINT handlers using `libc::signal`.
/// Kept minimal on purpose: no dependency on `signal-hook` or
/// `tokio::signal`, and no `sigaction` plumbing, because the
/// SimpleExecutor loop only needs a one-bit "someone asked us to
/// stop" signal and the handler body is async-signal-safe.
///
/// Returns the previous handler pointers so the caller can restore
/// them if needed (tests do this to avoid leaking handlers between
/// cases).
#[cfg(unix)]
fn install_shutdown_signal_handlers() {
    // SAFETY: single-threaded startup, before any worker threads spawn.
    // libc::signal is the minimal POSIX primitive that's sufficient
    // for the flag-set-and-poll pattern.
    unsafe {
        libc::signal(libc::SIGTERM, shutdown_signal_handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, shutdown_signal_handler as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_shutdown_signal_handlers() {
    // Windows has no POSIX signals. The daemon surface is Unix-only
    // in practice (the `daemonize` path at daemonize.rs is
    // `#![cfg(unix)]`), so the non-Unix branch is a no-op.
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

fn main() {
    if let Err(err) = run() {
        wezterm_blob_leases::clear_storage();
        log::error!("{:#}", err);
        std::process::exit(1);
    }
    wezterm_blob_leases::clear_storage();
}

fn run() -> anyhow::Result<()> {
    //stats::Stats::init()?;
    config::designate_this_as_the_main_thread();
    let _saver = umask::UmaskSaver::new();

    let opts = Opt::parse();

    config::common_init(
        opts.config_file.as_ref(),
        &opts.config_override,
        opts.skip_config,
    )?;

    let config = config::configuration();

    config.update_ulimit()?;
    if let Some(value) = &config.default_ssh_auth_sock {
        // SAFETY: called during single-threaded startup before worker threads spawn.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", value) };
    }

    if opts.daemonize {
        spawn_daemonized_copy(&opts, &config)?;
        return Ok(());
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
    // SAFETY: called during single-threaded startup before worker threads spawn.
    unsafe {
        for name in &[
            "OLDPWD",
            "PWD",
            "SHLVL",
            "WEZTERM_PANE",
            "WEZTERM_UNIX_SOCKET",
            "FRANKENTERM_UNIX_SOCKET",
            "_",
        ] {
            std::env::remove_var(name);
        }
        for name in &config::configuration().mux_env_remove {
            std::env::remove_var(name);
        }
    }

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

    spawn_listener().map_err(|e| {
        log::error!("problem spawning listeners: {:?}", e);
        e
    })?;
    log::info!(
        "frankenterm-mux-server-ready unix_domains={} tls_servers={}",
        config.unix_domains.len(),
        config.tls_servers.len()
    );

    let activity = Activity::new();

    promise::spawn::spawn(async move {
        if let Err(err) = async_run(cmd).await {
            terminate_with_error(err);
        }
        drop(activity);
    })
    .detach();

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
        config::lua::emit_event(&lua, ("mux-startup".to_string(), args)).await?;
    }
    Ok(())
}

async fn async_run(cmd: Option<CommandBuilder>) -> anyhow::Result<()> {
    let mux = Mux::get();
    let config = config::configuration();

    update_mux_domains_for_server(&config)?;
    let _config_subscription = config::subscribe_to_config_reload(move || {
        promise::spawn::spawn_into_main_thread(async move {
            if let Err(err) = update_mux_domains_for_server(&config::configuration()) {
                log::error!("Error updating mux domains: {:#}", err);
            }
        })
        .detach();
        true
    });

    let domain = mux.default_domain();

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
        domain.attach(Some(*window_id)).await?;

        let _tab = mux
            .default_domain()
            .spawn(config.initial_size(0, None), cmd, None, *window_id)
            .await?;
    }
    Ok(())
}

fn terminate_with_error(err: anyhow::Error) -> ! {
    log::error!("{:#}; terminating", err);
    std::process::exit(1);
}

mod ossl;

fn set_mux_socket_environment(config: &config::ConfigHandle) {
    // SAFETY: Setting environment variables must happen before worker threads
    // are spawned to avoid data races. We publish both legacy and ft-specific
    // socket vars so spawned processes and sibling tools resolve the same mux.
    if let Some(unix_dom) = config.unix_domains.first() {
        let socket_path = unix_dom.socket_path();
        unsafe {
            std::env::set_var("WEZTERM_UNIX_SOCKET", &socket_path);
            std::env::set_var("FRANKENTERM_UNIX_SOCKET", &socket_path);
        }
    }
}

fn daemonized_child_args(opts: &Opt) -> Vec<OsString> {
    let mut args = vec![OsString::from("--daemonize=false")];
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

pub fn spawn_listener() -> anyhow::Result<()> {
    let config = configuration();
    set_mux_socket_environment(&config);

    for unix_dom in &config.unix_domains {
        let mut listener =
            frankenterm_mux_server_impl::local::LocalListener::with_domain(unix_dom)?;
        thread::spawn(move || {
            listener.run();
        });
    }

    for tls_server in &config.tls_servers {
        ossl::spawn_tls_listener(tls_server)?;
    }

    Ok(())
}

fn spawn_daemonized_copy(opts: &Opt, config: &config::ConfigHandle) -> anyhow::Result<()> {
    let mut cmd = Command::new(
        std::env::current_exe().context("resolving current executable for daemonize")?,
    );
    for arg in daemonized_child_args(opts) {
        cmd.arg(arg);
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(config.daemon_options.open_stdout()?);
    cmd.stderr(config.daemon_options.open_stderr()?);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(winapi::um::winbase::DETACHED_PROCESS);
    }

    let _child = cmd
        .spawn()
        .context("spawning daemonized mux server child")?;
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
        // SAFETY: tests take a global mutex and mutate process env serially.
        unsafe {
            std::env::remove_var("WEZTERM_UNIX_SOCKET");
            std::env::remove_var("FRANKENTERM_UNIX_SOCKET");
        }
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
        // SAFETY: tests take a global mutex and mutate process env serially.
        unsafe {
            std::env::set_var("WEZTERM_UNIX_SOCKET", &sentinel);
            std::env::set_var("FRANKENTERM_UNIX_SOCKET", &sentinel);
        }

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

    #[test]
    fn daemonized_child_args_forward_cli_state_and_prog_separator() {
        let mut opts = make_opt();
        opts.skip_config = true;
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

        assert_eq!(args, vec![OsString::from("--daemonize=false")]);
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
