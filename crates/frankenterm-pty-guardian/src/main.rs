#![forbid(unsafe_code)]

#[cfg(unix)]
use clap::{Args, Parser, Subcommand};
#[cfg(unix)]
use frankenterm_pty_guardian::{
    GuardianClient, GuardianService, GuardianServiceConfig, ProvisionTokenOutcome,
    provision_guardian_token,
};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(
    name = "frankenterm-pty-guardian",
    about = "Opt-in standalone owner of FrankenTerm native PTYs"
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

    /// Maximum simultaneously connected clients.
    #[arg(long, default_value_t = 64)]
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
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Serve(args) => {
            let config = GuardianServiceConfig::new(
                args.endpoint.socket_path,
                args.endpoint.token_path,
                args.max_connections,
                args.max_panes,
                args.max_output_bytes_per_pane,
                args.max_total_output_bytes,
                Duration::from_millis(args.poll_interval_ms),
            )?;
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
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("frankenterm-pty-guardian is supported only on Unix");
    std::process::exit(2);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
}
