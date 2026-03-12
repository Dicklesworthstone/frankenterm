use anyhow::Context;
use clap::*;
use config::configuration;
use frankenterm_mux_server_impl::update_mux_domains_for_server;
use mux::Mux;
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain};
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use wezterm_gui_subcommands::*;

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

    loop {
        executor.tick()?;
    }
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
}
