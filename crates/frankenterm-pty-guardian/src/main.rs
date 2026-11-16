#![forbid(unsafe_code)]

#[cfg(unix)]
use clap::{Args, Parser, Subcommand};
#[cfg(unix)]
use frankenterm_pty_guardian::{
    BrokerControlServiceConfigV1, BrokerControlServiceV1, GuardianClient, GuardianService,
    GuardianServiceConfig, ProvisionTokenOutcome, guardian_runtime_build_identity,
    provision_guardian_token, run_broker_exec_bootstrap,
};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;

const GUARDIAN_ATOMIC_COMPONENT_MARKER: &str = env!("FT_ATOMIC_COMPONENT_MARKER");

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(
    name = "frankenterm-pty-guardian",
    about = "Opt-in standalone owner of FrankenTerm native PTYs",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the foreground guardian service.
    Serve(ServeArgs),
    /// Authenticate and traverse one complete bounded pane census.
    Probe(EndpointArgs),
    /// Create a durable private token, or validate the safe existing token.
    ProvisionToken(TokenArgs),
    /// Stop the guardian only if it currently owns no panes.
    GuardedStop(EndpointArgs),
    /// Run the separately supervised same-binary PTY broker process.
    #[command(name = "broker-serve", hide = true)]
    BrokerServe(BrokerServeArgs),
    /// Same-binary child held behind one authenticated durable Spawn barrier.
    #[command(name = "exec-bootstrap", hide = true)]
    ExecBootstrap(ExecBootstrapArgs),
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct EndpointArgs {
    /// Absolute path to the guardian Unix socket.
    #[arg(long)]
    socket_path: PathBuf,

    /// Absolute path to an existing owner-only 32-byte authentication token.
    #[arg(long)]
    token_path: PathBuf,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct TokenArgs {
    /// Absolute path to the owner-only 32-byte authentication token.
    #[arg(long)]
    token_path: PathBuf,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    endpoint: EndpointArgs,

    /// Maximum simultaneously connected clients. A mux holds three per pane
    /// (lease, replay, checkpoint publisher) and a few more for its census
    /// and spawns, so the transport's maximum serves about 340 panes; at 64
    /// a mux's 21st pane could not connect (ft-bxeig).
    #[arg(long, default_value_t = 1024)]
    max_connections: usize,

    /// Maximum PTYs owned by this guardian process.
    #[arg(long, default_value_t = 4096)]
    max_panes: usize,

    /// Maximum unread PTY output retained per pane before readiness is paused.
    #[arg(long, default_value_t = 1024 * 1024)]
    max_output_bytes_per_pane: usize,

    /// Maximum unread PTY output retained across the whole guardian.
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_total_output_bytes: usize,

    /// Readiness and child-reap cadence in milliseconds.
    #[arg(long, default_value_t = 25)]
    poll_interval_ms: u64,

    /// Connect Genesis transactions to this separately supervised broker.
    #[arg(long, requires = "broker_token_path")]
    broker_socket_path: Option<PathBuf>,

    /// Existing private token for the configured broker endpoint.
    #[arg(long, requires = "broker_socket_path")]
    broker_token_path: Option<PathBuf>,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct BrokerServeArgs {
    #[command(flatten)]
    endpoint: EndpointArgs,

    /// Absolute path to an existing owner-only broker Spawn catalog.
    #[arg(long)]
    spawn_catalog_path: PathBuf,

    /// Absolute path to an existing owner-only broker pane-lease catalog.
    #[arg(long)]
    lease_catalog_path: PathBuf,

    /// Maximum simultaneously connected successor guardians.
    #[arg(long, default_value_t = 64)]
    max_connections: usize,

    /// Broker readiness cadence in milliseconds.
    #[arg(long, default_value_t = 25)]
    poll_interval_ms: u64,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct ExecBootstrapArgs {
    /// Absolute path to the existing owner-only guardian authentication token.
    #[arg(long)]
    token_path: PathBuf,

    /// Canonical content-free identity of the exact durable Spawn attempt.
    #[arg(long)]
    identity: String,
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    retain_guardian_atomic_component_identity()?;
    // Warnings (a slow checkpoint step, say) reach the service log; the
    // guardian never logs terminal content.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .init();
    let args = Cli::parse();
    match args.command {
        Command::Serve(args) => {
            raise_open_file_limit();
            let mut config = GuardianServiceConfig::new(
                args.endpoint.socket_path,
                args.endpoint.token_path,
                args.max_connections,
                args.max_panes,
                args.max_output_bytes_per_pane,
                args.max_total_output_bytes,
                Duration::from_millis(args.poll_interval_ms),
            )?;
            if let (Some(socket_path), Some(token_path)) =
                (args.broker_socket_path, args.broker_token_path)
            {
                config = config.with_broker_endpoint(socket_path, token_path)?;
            }
            let mut service = GuardianService::bind(config)?;
            service.run_forever()?;
        }
        Command::Probe(endpoint) => {
            let mut client = GuardianClient::connect(
                &endpoint.socket_path,
                &endpoint.token_path,
                uuid::Uuid::new_v4(),
            )?;
            let report = client.probe()?;
            println!(
                "guardian-ready incarnation={} panes={}",
                report.guardian_incarnation, report.pane_count
            );
        }
        Command::ProvisionToken(args) => match provision_guardian_token(&args.token_path)? {
            ProvisionTokenOutcome::Created => println!("guardian-token-created"),
            ProvisionTokenOutcome::Existing => println!("guardian-token-existing"),
        },
        Command::GuardedStop(endpoint) => {
            let mut client = GuardianClient::connect(
                &endpoint.socket_path,
                &endpoint.token_path,
                uuid::Uuid::new_v4(),
            )?;
            client.guarded_stop(uuid::Uuid::new_v4(), uuid::Uuid::new_v4())?;
            println!("guardian-stop-accepted");
        }
        Command::BrokerServe(args) => {
            raise_open_file_limit();
            let config = BrokerControlServiceConfigV1::new(
                args.endpoint.socket_path,
                args.endpoint.token_path,
                args.spawn_catalog_path,
                args.lease_catalog_path,
                guardian_runtime_build_identity()?,
                args.max_connections,
                Duration::from_millis(args.poll_interval_ms),
            )?;
            let mut service = BrokerControlServiceV1::bind(config)?;
            service.run_forever()?;
        }
        Command::ExecBootstrap(args) => {
            run_broker_exec_bootstrap(&args.token_path, &args.identity)?;
        }
    }
    Ok(())
}

/// The soft open-file limit the guardian and the broker run with: the mux's
/// own `ulimit_nofile` default, so a guardian pane's child inherits what a
/// plain pane's child does.
#[cfg(unix)]
const OPEN_FILE_SOFT_LIMIT: nix::sys::resource::rlim_t = 2048;

/// Raise the soft open-file limit to [`OPEN_FILE_SOFT_LIMIT`], within the
/// hard limit (ft-bxeig). Every pane holds descriptors in both processes (the
/// guardian its connections and journals, the broker its PTY and journals),
/// and a launchd agent starts with a soft limit of 256. Never lowers it.
#[cfg(unix)]
fn raise_open_file_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else {
        return;
    };
    let wanted = OPEN_FILE_SOFT_LIMIT.min(hard);
    if soft < wanted
        && let Err(error) = setrlimit(Resource::RLIMIT_NOFILE, wanted, hard)
    {
        log::warn!("could not raise the open-file limit from {soft} to {wanted}: {error}");
    }
}

fn retain_guardian_atomic_component_identity() -> anyhow::Result<()> {
    let marker_identity = frankenterm_build_identity::parse_atomic_component_marker(
        GUARDIAN_ATOMIC_COMPONENT_MARKER,
        frankenterm_build_identity::AtomicComponentRole::FrankenTermPtyGuardian,
    )?;
    anyhow::ensure!(
        marker_identity == frankenterm_pty_guardian::guardian_embedded_build_identity()?,
        "guardian process marker differs from compiled library build identity"
    );
    std::hint::black_box(GUARDIAN_ATOMIC_COMPONENT_MARKER);
    std::hint::black_box(marker_identity);
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    if let Err(error) = retain_guardian_atomic_component_identity() {
        eprintln!("invalid embedded guardian atomic component identity: {error}");
        std::process::exit(2);
    }
    eprintln!("frankenterm-pty-guardian is supported only on Unix");
    std::process::exit(2);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn command_surface_requires_explicit_lifecycle_transaction() {
        let cli = Cli::try_parse_from([
            "frankenterm-pty-guardian",
            "serve",
            "--socket-path",
            "/private/tmp/guardian/socket",
            "--token-path",
            "/private/tmp/guardian/token",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Serve(_)));
        assert!(
            Cli::try_parse_from([
                "frankenterm-pty-guardian",
                "--socket-path",
                "/private/tmp/guardian/socket",
                "--token-path",
                "/private/tmp/guardian/token",
            ])
            .is_err()
        );

        for (name, expected) in [("probe", "probe"), ("guarded-stop", "guarded-stop")] {
            let cli = Cli::try_parse_from([
                "frankenterm-pty-guardian",
                name,
                "--socket-path",
                "/private/tmp/guardian/socket",
                "--token-path",
                "/private/tmp/guardian/token",
            ])
            .unwrap();
            assert_eq!(
                match cli.command {
                    Command::Probe(_) => "probe",
                    Command::GuardedStop(_) => "guarded-stop",
                    _ => "unexpected",
                },
                expected
            );
        }

        let provision = Cli::try_parse_from([
            "frankenterm-pty-guardian",
            "provision-token",
            "--token-path",
            "/private/tmp/guardian/token",
        ])
        .unwrap();
        assert!(matches!(provision.command, Command::ProvisionToken(_)));
    }

    #[test]
    fn version_probe_is_non_mutating_and_uses_the_package_version() {
        let error = Cli::try_parse_from(["frankenterm-pty-guardian", "--version"])
            .expect_err("--version must terminate parsing before command execution");
        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        let rendered = error.to_string();
        assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
        assert!(!rendered.contains("--socket-path"));
        assert!(!rendered.contains("--token-path"));
    }

    #[test]
    fn serve_defaults_admit_a_fleet_host_of_panes() {
        let cli = Cli::try_parse_from([
            "frankenterm-pty-guardian",
            "serve",
            "--socket-path",
            "/private/tmp/guardian/socket",
            "--token-path",
            "/private/tmp/guardian/token",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("serve parses as serve");
        };
        // Three connections per mux pane, a mux census and a few spawns, for
        // a fleet host's 40 panes and a successor mux holding them as well.
        assert!(args.max_connections >= 2 * (3 * 40 + 8));

        // The limit is only ever raised, and stays within the hard limit.
        use nix::sys::resource::{Resource, getrlimit};
        let (before, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        raise_open_file_limit();
        let (after, after_hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        assert_eq!(after_hard, hard);
        assert!(after >= before);
        assert!(after >= OPEN_FILE_SOFT_LIMIT.min(hard));
    }

    #[test]
    fn binary_startup_retains_and_validates_the_component_identity() {
        retain_guardian_atomic_component_identity().unwrap();
        assert!(GUARDIAN_ATOMIC_COMPONENT_MARKER.contains(":frankenterm-pty-guardian:"));
        assert!(
            frankenterm_build_identity::parse_atomic_component_marker(
                GUARDIAN_ATOMIC_COMPONENT_MARKER,
                frankenterm_build_identity::AtomicComponentRole::FrankenTermMuxServer,
            )
            .is_err()
        );
    }
}
