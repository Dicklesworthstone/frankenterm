//! `frankenterm-gui cli`: talk to a running mux over its unix socket.
//!
//! This is a small subset of upstream `wezterm cli` (spawn, list,
//! move-pane-to-new-tab and move-tab). It needs
//! no display, so an agent running inside a pane can open a tab in the window
//! it lives in.

use anyhow::{Context, anyhow};
use clap::{Parser, ValueHint};
use codec::{
    DomainBindingId, ListPanesResponse, MovePaneToNewTab, OrderedWindowStateV1, RemoteTabId,
    SpawnV2, WindowOrderRevision,
};
use config::keyassignment::SpawnTabDomain;
use frankenterm_client::client::Client;
use frankenterm_client::ordered_reorder::{
    self, AcceptedSnapshot, DesiredWindowOrder, MutationNamespace, ReorderDecision,
    WindowOrderCommitOutcome, WindowOrderIntent,
};
use mux::tab::PaneEntry;
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use promise::spawn::block_on;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Parser, Clone)]
pub struct CliCommand {
    #[command(subcommand)]
    sub: CliSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum CliSubCommand {
    /// Spawn a command into a new tab or window and print the new pane id.
    #[command(name = "spawn")]
    Spawn(SpawnArgs),

    /// List windows, tabs and panes.
    #[command(name = "list")]
    List(ListArgs),

    /// Move a pane into a new tab, in an existing window or a new one.
    #[command(name = "move-pane-to-new-tab")]
    MovePaneToNewTab(MovePaneToNewTabArgs),

    /// Move a tab to another position in its window, committed on the mux
    /// server, and print the window's committed tab order.
    #[command(name = "move-tab")]
    MoveTab(MoveTabArgs),
}

#[derive(Debug, Parser, Clone)]
struct SpawnArgs {
    /// The pane whose window gets the new tab. Defaults to $WEZTERM_PANE,
    /// then to the focused pane of the most recently active client. A pane
    /// that does not exist is an error. With no pane at all, the tab goes to
    /// the newest window of the workspace.
    #[arg(long)]
    pane_id: Option<mux::pane::PaneId>,

    /// Put the new tab in this window instead.
    #[arg(long, conflicts_with = "new_window")]
    window_id: Option<WindowId>,

    /// Open a new window instead of a tab.
    #[arg(long)]
    new_window: bool,

    /// Working directory for the spawned program.
    #[arg(long, value_parser, value_hint = ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// Spawn into this domain instead of the default domain.
    #[arg(long)]
    domain_name: Option<String>,

    /// Workspace for a new window. Defaults to the source pane's workspace.
    #[arg(long)]
    workspace: Option<String>,

    /// The program to run, with its arguments. Defaults to the domain's
    /// default program.
    #[arg(value_parser, value_hint = ValueHint::CommandWithArguments, num_args = 1.., last = true)]
    prog: Vec<OsString>,
}

#[derive(Debug, Parser, Clone)]
struct MovePaneToNewTabArgs {
    /// The pane to move. Defaults to $WEZTERM_PANE, then to the focused pane
    /// of the most recently active client.
    #[arg(long)]
    pane_id: Option<mux::pane::PaneId>,

    /// Put the new tab in this window. Defaults to the pane's own window.
    #[arg(long, conflicts_with = "new_window")]
    window_id: Option<WindowId>,

    /// Put the new tab in a new window.
    #[arg(long)]
    new_window: bool,

    /// Workspace for a new window. Defaults to the pane's workspace.
    #[arg(long)]
    workspace: Option<String>,
}

#[derive(Debug, Parser, Clone)]
struct MoveTabArgs {
    /// The tab to move, as `cli list` shows it.
    #[arg(long)]
    tab_id: u64,

    /// The new zero-based position. A position past the end means last.
    #[arg(long)]
    index: usize,

    /// Send one compare-and-set against this window order revision, with no
    /// rebase or retry, and print the server's decision. A stale revision
    /// shows the conflict path.
    #[arg(long)]
    expected_revision: Option<u64>,
}

#[derive(Debug, Parser, Clone)]
struct ListArgs {
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
}

pub fn run_cli(cmd: CliCommand) -> anyhow::Result<()> {
    let client = connect()?;
    let executor = promise::spawn::ScopedExecutor::new();
    block_on(executor.run(async move {
        let ui = mux::connui::ConnectionUI::new_headless();
        client
            .verify_version_compat(&ui)
            .await
            .context("negotiate protocol with the mux server")?;
        let result = match cmd.sub {
            CliSubCommand::Spawn(args) => run_spawn(&client, args).await,
            CliSubCommand::List(args) => run_list(&client, args).await,
            CliSubCommand::MovePaneToNewTab(args) => run_move_pane_to_new_tab(&client, args).await,
            CliSubCommand::MoveTab(args) => run_move_tab(&client, args).await,
        };
        // Dropping the client makes its reconnect thread log a spurious
        // "won't try to reconnect" error, so leave before that happens.
        use std::io::Write;
        std::io::stdout().flush().ok();
        match result {
            Ok(()) => std::process::exit(0),
            Err(err) => {
                eprintln!("frankenterm-gui cli: {err:#}");
                std::process::exit(1)
            }
        }
    }))
}

fn connect() -> anyhow::Result<Client> {
    let mut ui = mux::connui::ConnectionUI::new_headless();
    let explicit = std::env::var_os("FRANKENTERM_UNIX_SOCKET")
        .or_else(|| std::env::var_os("WEZTERM_UNIX_SOCKET"))
        .filter(|p| !p.is_empty());
    let client = match explicit {
        Some(path) => {
            let dom = config::UnixDomain {
                socket_path: Some(path.into()),
                no_serve_automatically: true,
                ..Default::default()
            };
            Client::new_unix_domain_request_only(&dom, &mut ui, true)
        }
        None => Client::new_default_unix_domain_request_only(
            &mut ui,
            true,
            true,
            &crate::termwindow::get_window_class(),
        ),
    };
    client.context("connect to the FrankenTerm mux socket")
}

/// Every pane in a listing: the tiled panes of each tab tree, then the
/// floating panes, which the listing carries separately.
fn pane_entries(panes: ListPanesResponse) -> Vec<PaneEntry> {
    let mut out = vec![];
    for tabroot in panes.tabs {
        let mut cursor = tabroot.into_tree().cursor();
        loop {
            if let Some(entry) = cursor.leaf_mut() {
                out.push(entry.clone());
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }
    }
    out.extend(
        panes
            .floating_panes
            .into_iter()
            .map(|floating| floating.pane),
    );
    out
}

/// Where the pane a command acts from came from, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceOrigin {
    PaneIdArg,
    WeztermPaneEnv,
    FocusedPane,
}

impl std::fmt::Display for SourceOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PaneIdArg => "--pane-id",
            Self::WeztermPaneEnv => "$WEZTERM_PANE",
            Self::FocusedPane => "the focused pane",
        })
    }
}

fn parse_env_pane(value: &str) -> anyhow::Result<mux::pane::PaneId> {
    value
        .trim()
        .parse()
        .map_err(|_| anyhow!("$WEZTERM_PANE is {value:?}, which is not a pane id"))
}

/// The pane the caller asked to act from: `--pane-id`, then
/// `$WEZTERM_PANE`, then the focused pane of the most recently active
/// client. `None` only when none of those exists. An RPC error is returned,
/// not treated as "no pane".
async fn requested_source(
    client: &Client,
    arg: Option<mux::pane::PaneId>,
) -> anyhow::Result<Option<(mux::pane::PaneId, SourceOrigin)>> {
    if let Some(pane_id) = arg {
        return Ok(Some((pane_id, SourceOrigin::PaneIdArg)));
    }
    if let Some(value) = std::env::var_os("WEZTERM_PANE").filter(|v| !v.is_empty()) {
        let value = value
            .into_string()
            .map_err(|v| anyhow!("$WEZTERM_PANE is {v:?}, which is not a pane id"))?;
        return Ok(Some((
            parse_env_pane(&value)?,
            SourceOrigin::WeztermPaneEnv,
        )));
    }
    let mut clients = client
        .list_clients()
        .await
        .context("ask the mux which pane is focused")?
        .clients;
    clients.retain(|c| c.focused_pane_id.is_some());
    clients.sort_by_key(|c| std::cmp::Reverse(c.last_input));
    Ok(clients
        .first()
        .and_then(|c| c.focused_pane_id)
        .map(|pane_id| (pane_id, SourceOrigin::FocusedPane)))
}

/// Look up a requested source pane. A pane that was asked for but is not in
/// the listing is an error, never a silent fallback.
fn find_source(
    entries: &[PaneEntry],
    requested: Option<(mux::pane::PaneId, SourceOrigin)>,
) -> anyhow::Result<Option<&PaneEntry>> {
    let Some((pane_id, origin)) = requested else {
        return Ok(None);
    };
    entries
        .iter()
        .find(|e| e.pane_id == pane_id)
        .map(Some)
        .ok_or_else(|| anyhow!("pane {pane_id} (from {origin}) does not exist"))
}

/// The newest window in `workspace`, used when no source pane exists.
fn newest_window_in_workspace(entries: &[PaneEntry], workspace: &str) -> Option<WindowId> {
    entries
        .iter()
        .filter(|e| e.workspace == workspace)
        .map(|e| e.window_id)
        .max()
}

async fn run_spawn(client: &Client, args: SpawnArgs) -> anyhow::Result<()> {
    let config = config::configuration();
    let entries = pane_entries(client.list_panes().await?);

    let source = find_source(&entries, requested_source(client, args.pane_id).await?)?;
    let target_window = match args.window_id {
        Some(id) => Some(
            entries
                .iter()
                .find(|e| e.window_id == id)
                .ok_or_else(|| anyhow!("window {id} does not exist"))?,
        ),
        None => None,
    };

    let workspace = args
        .workspace
        .or_else(|| target_window.or(source).map(|e| e.workspace.clone()))
        .or_else(|| config.default_workspace.clone())
        .unwrap_or_else(|| mux::DEFAULT_WORKSPACE.to_string());

    let window_id = if args.new_window {
        None
    } else if let Some(target) = target_window {
        Some(target.window_id)
    } else if let Some(source) = source {
        Some(source.window_id)
    } else {
        newest_window_in_workspace(&entries, &workspace)
    };

    let command_dir = match args.cwd {
        Some(cwd) => {
            let path = PathBuf::from(cwd);
            let path = if path.is_relative() {
                std::env::current_dir()
                    .context("resolve current directory")?
                    .join(path)
            } else {
                path
            };
            Some(
                path.to_str()
                    .ok_or_else(|| anyhow!("--cwd {} is not valid UTF-8", path.display()))?
                    .to_string(),
            )
        }
        None => None,
    };

    let command = if args.prog.is_empty() {
        None
    } else {
        Some(CommandBuilder::from_argv(args.prog))
    };

    let size = entries
        .iter()
        .find(|e| Some(e.window_id) == window_id)
        .map(|e| e.size)
        .unwrap_or_else(|| config.initial_size(0, None));

    let spawned = client
        .spawn_v2(SpawnV2 {
            domain: args
                .domain_name
                .map_or(SpawnTabDomain::DefaultDomain, SpawnTabDomain::DomainName),
            window_id,
            command,
            command_dir,
            size,
            workspace,
        })
        .await?;
    println!("{}", spawned.pane_id);
    Ok(())
}

async fn run_move_pane_to_new_tab(
    client: &Client,
    args: MovePaneToNewTabArgs,
) -> anyhow::Result<()> {
    let entries = pane_entries(client.list_panes().await?);
    let source =
        find_source(&entries, requested_source(client, args.pane_id).await?)?.ok_or_else(|| {
            anyhow!("no pane to move: pass --pane-id, or run inside a FrankenTerm pane")
        })?;
    let pane_id = source.pane_id;

    let window_id = if args.new_window {
        None
    } else {
        let id = args.window_id.unwrap_or(source.window_id);
        if !entries.iter().any(|e| e.window_id == id) {
            anyhow::bail!("window {id} does not exist");
        }
        Some(id)
    };

    let moved = client
        .move_pane_to_new_tab(MovePaneToNewTab {
            pane_id,
            window_id,
            workspace_for_new_window: args.workspace.or_else(|| Some(source.workspace.clone())),
        })
        .await?;
    println!("{}\t{}", moved.window_id, moved.tab_id);
    Ok(())
}

/// `window <id> revision <n>: <tab ids in order>`.
fn describe_window_order(window: &OrderedWindowStateV1) -> String {
    let tabs: Vec<String> = window
        .ordered_tab_ids
        .iter()
        .map(|tab| tab.get().to_string())
        .collect();
    format!(
        "window {} revision {}: {}",
        window.window_id.get(),
        window.order_revision.get(),
        tabs.join(" ")
    )
}

/// A fresh binding for each run. The CLI owns no durable layout, so the
/// server must not mistake two runs for one attachment.
fn random_binding() -> DomainBindingId {
    // A v4 UUID carries fixed version bits, so it is never the reserved zero.
    DomainBindingId::from_bytes(uuid::Uuid::new_v4().into_bytes())
}

async fn run_move_tab(client: &Client, args: MoveTabArgs) -> anyhow::Result<()> {
    let tab_id = RemoteTabId::new(args.tab_id);
    let binding = random_binding();
    if let Some(revision) = args.expected_revision {
        return run_single_reorder_cas(client, binding, tab_id, args.index, revision).await;
    }
    let namespace = MutationNamespace::random()?;
    let intent = WindowOrderIntent {
        domain_binding_id: binding,
        expected_session: None,
        desired: DesiredWindowOrder::MoveTab {
            tab_id,
            index: args.index,
        },
    };
    let outcome = ordered_reorder::commit_window_order(client, &namespace, &intent)
        .await
        .context("commit the tab order on the mux server")?;
    match outcome {
        WindowOrderCommitOutcome::Applied {
            window, rebased, ..
        } => {
            let how = if rebased {
                "applied (rebased)"
            } else {
                "applied"
            };
            println!("{how}: {}", describe_window_order(&window));
            Ok(())
        }
        WindowOrderCommitOutcome::Unchanged { window } => {
            println!("unchanged: {}", describe_window_order(&window));
            Ok(())
        }
        WindowOrderCommitOutcome::ServerWins { window, reason } => {
            println!("server kept its order: {}", describe_window_order(&window));
            anyhow::bail!("the move was not applied: {reason:?}")
        }
        WindowOrderCommitOutcome::UnknownTab(_) => {
            anyhow::bail!("tab {} is in no server window", args.tab_id)
        }
        other => anyhow::bail!("the move was not applied: {other:?}"),
    }
}

/// One PDU88 with a caller-chosen expected revision, for testing the
/// server's compare-and-set. Nothing is refreshed, rebased or retried.
async fn run_single_reorder_cas(
    client: &Client,
    binding: DomainBindingId,
    tab_id: RemoteTabId,
    index: usize,
    expected_revision: u64,
) -> anyhow::Result<()> {
    let list = ordered_reorder::ordered_list_request(binding);
    let response = client.list_panes_ordered_v1(list.clone()).await?;
    let (session, snapshot) = match ordered_reorder::accept_ordered_snapshot(&list, response)? {
        AcceptedSnapshot::Ready { session, snapshot } => (session, snapshot),
        AcceptedSnapshot::Unavailable(unavailable) => {
            anyhow::bail!("the server gave no ordered snapshot: {unavailable:?}")
        }
    };
    let mut window = snapshot
        .ordered_windows
        .into_iter()
        .find(|window| window.ordered_tab_ids.contains(&tab_id))
        .ok_or_else(|| anyhow!("tab {} is in no server window", tab_id.get()))?;
    let desired = ordered_reorder::move_tab_order(&window.ordered_tab_ids, tab_id, index)
        .ok_or_else(|| anyhow!("tab {} is in no server window", tab_id.get()))?;
    window.order_revision = WindowOrderRevision::new(expected_revision);
    let mutation_id = MutationNamespace::random()?
        .next_id()
        .ok_or_else(|| anyhow!("a fresh mutation namespace has no ids"))?;
    let request = ordered_reorder::build_reorder_request(&session, &window, desired, mutation_id)?;
    let response = client.reorder_window_tabs_v1(request.clone()).await?;
    match ordered_reorder::correlate_reorder_response(&request, &response)? {
        ReorderDecision::Applied { commit, .. } => {
            println!("applied: {}", describe_window_order(&commit.window));
            Ok(())
        }
        ReorderDecision::Conflict { commit, .. } => {
            println!("conflict: {}", describe_window_order(&commit.window));
            anyhow::bail!("revision {expected_revision} is stale")
        }
        other => anyhow::bail!("the server rejected the move: {other:?}"),
    }
}

async fn run_list(client: &Client, args: ListArgs) -> anyhow::Result<()> {
    let entries = pane_entries(client.list_panes().await?);
    if args.json {
        let rows: Vec<_> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "window_id": e.window_id,
                    "tab_id": e.tab_id,
                    "pane_id": e.pane_id,
                    "workspace": e.workspace,
                    "size": { "rows": e.size.rows, "cols": e.size.cols },
                    "title": e.title,
                    "cwd": e.working_dir.as_ref().map(|u| u.as_str().to_string()),
                    "is_active": e.is_active_pane,
                    "tty_name": e.tty_name,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("WINID\tTABID\tPANEID\tWORKSPACE\tSIZE\tTITLE\tCWD");
    for e in &entries {
        println!(
            "{}\t{}\t{}\t{}\t{}x{}\t{}\t{}",
            e.window_id,
            e.tab_id,
            e.pane_id,
            e.workspace,
            e.size.cols,
            e.size.rows,
            e.title,
            e.working_dir
                .as_ref()
                .map(|u| u.as_str().to_string())
                .unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux::renderable::StableCursorPosition;
    use mux::tab::PaneNode;
    use std::collections::HashMap;
    use wezterm_term::TerminalSize;

    fn entry(window_id: WindowId, tab_id: usize, pane_id: usize, workspace: &str) -> PaneEntry {
        PaneEntry {
            window_id,
            tab_id,
            pane_id,
            title: format!("pane {pane_id}"),
            size: TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: workspace.to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    #[test]
    fn pane_entries_include_floating_panes() {
        let listing = ListPanesResponse {
            tabs: vec![PaneNode::Leaf(entry(1, 10, 100, "default"))],
            tab_titles: vec!["tab".to_string()],
            window_titles: HashMap::new(),
            floating_panes: vec![codec::FloatingPaneSnapshotEntry {
                pane: entry(1, 10, 101, "default"),
                rect: mux::tab::FloatingPaneRect {
                    left: 2,
                    top: 2,
                    width: 20,
                    height: 8,
                },
                z_order: 0,
                visible: true,
                pinned: false,
                opacity: 1.0,
                focused: false,
            }],
        };
        let ids: Vec<_> = pane_entries(listing).iter().map(|e| e.pane_id).collect();
        assert_eq!(ids, vec![100, 101]);
    }

    #[test]
    fn a_requested_pane_that_does_not_exist_is_an_error() {
        let entries = vec![entry(1, 10, 100, "default")];
        for origin in [
            SourceOrigin::PaneIdArg,
            SourceOrigin::WeztermPaneEnv,
            SourceOrigin::FocusedPane,
        ] {
            let err = find_source(&entries, Some((999, origin)))
                .expect_err("an unknown pane must not fall back");
            assert!(
                err.to_string().contains("pane 999"),
                "unexpected error: {err:#}"
            );
        }
        assert_eq!(
            find_source(&entries, Some((100, SourceOrigin::PaneIdArg)))
                .unwrap()
                .map(|e| e.pane_id),
            Some(100)
        );
        assert!(find_source(&entries, None).unwrap().is_none());
    }

    #[test]
    fn a_malformed_wezterm_pane_is_an_error() {
        assert_eq!(parse_env_pane("42").unwrap(), 42);
        assert!(parse_env_pane("not-a-pane").is_err());
    }

    #[test]
    fn the_fallback_window_stays_in_the_workspace() {
        let entries = vec![
            entry(1, 10, 100, "default"),
            entry(3, 30, 300, "default"),
            entry(9, 90, 900, "ops"),
        ];
        assert_eq!(newest_window_in_workspace(&entries, "default"), Some(3));
        assert_eq!(newest_window_in_workspace(&entries, "ops"), Some(9));
        assert_eq!(newest_window_in_workspace(&entries, "empty"), None);
    }

    #[test]
    fn a_window_order_prints_its_tabs_in_order() {
        let window = OrderedWindowStateV1 {
            window_id: codec::RemoteWindowId::new(2),
            order_revision: WindowOrderRevision::new(5),
            ordered_tab_ids: [7, 3, 9].into_iter().map(RemoteTabId::new).collect(),
            active_tab_id: Some(RemoteTabId::new(3)),
        };
        assert_eq!(describe_window_order(&window), "window 2 revision 5: 7 3 9");
    }

    #[test]
    fn each_run_gets_a_fresh_nonzero_binding() {
        let first = random_binding();
        assert_ne!(first.as_bytes(), [0; 16]);
        assert_ne!(first, random_binding());
    }

    #[test]
    fn move_tab_parses_its_arguments() {
        let cmd = CliCommand::try_parse_from([
            "cli",
            "move-tab",
            "--tab-id",
            "4",
            "--index",
            "0",
            "--expected-revision",
            "1",
        ])
        .unwrap();
        let CliSubCommand::MoveTab(args) = cmd.sub else {
            panic!("expected move-tab, got {:?}", cmd.sub);
        };
        assert_eq!(args.tab_id, 4);
        assert_eq!(args.index, 0);
        assert_eq!(args.expected_revision, Some(1));
    }
}
