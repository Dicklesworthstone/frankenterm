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

pub fn spawn_listener() -> anyhow::Result<()> {
    let config = configuration();
    for unix_dom in &config.unix_domains {
        // SAFETY: called during single-threaded startup of the listener.
        unsafe { std::env::set_var("WEZTERM_UNIX_SOCKET", unix_dom.socket_path()) };
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
    cmd.arg("--daemonize=false");
    if opts.skip_config {
        cmd.arg("-n");
    }
    if let Some(f) = &opts.config_file {
        cmd.arg("--config-file");
        cmd.arg(f);
    }
    for (name, value) in &opts.config_override {
        cmd.arg("--config");
        cmd.arg(format!("{name}={value}"));
    }
    if let Some(cwd) = &opts.cwd {
        cmd.arg("--cwd");
        cmd.arg(cwd);
    }
    if !opts.prog.is_empty() {
        cmd.arg("--");
        for a in &opts.prog {
            cmd.arg(a);
        }
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
