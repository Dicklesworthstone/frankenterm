//! `frankenterm-gui cli`: send one request to a running FrankenTerm mux and
//! exit.
//!
//! This is the `list`, `spawn` and `move-pane-to-new-tab` subset of
//! `wezterm cli`, with the same arguments and output, so a script written
//! for one works with the other. Stock `wezterm cli` cannot talk to a
//! FrankenTerm mux because the protocols differ. It needs no display, which
//! lets a script or agent running inside a pane open tabs in its own window.
//!
//! The socket is `$FRANKENTERM_UNIX_SOCKET`, else `$WEZTERM_UNIX_SOCKET`
//! (both are set inside FrankenTerm panes), else a running GUI's socket, else
//! the first configured unix domain. The command never starts a mux server.

use anyhow::{Context, anyhow};
use clap::{Parser, ValueEnum, ValueHint};
use codec::{ListPanesResponse, MovePaneToNewTab, SpawnV2};
use config::keyassignment::SpawnTabDomain;
use frankenterm_client::client::Client;
use mux::client::ClientInfo;
use mux::pane::PaneId;
use mux::tab::{PaneEntry, TabId};
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use tabout::{Alignment, Column, tabulate_output};

#[derive(Debug, Parser, Clone)]
pub struct CliCommand {
    /// When neither $FRANKENTERM_UNIX_SOCKET nor $WEZTERM_UNIX_SOCKET is
    /// set, connect to the mux server's socket rather than a running GUI's.
    #[arg(long = "prefer-mux")]
    prefer_mux: bool,

    /// The `--class` the GUI was started with, if any, so that the right
    /// GUI instance is found.
    #[arg(long = "class")]
    class: Option<String>,

    #[command(subcommand)]
    sub: CliSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum CliSubCommand {
    /// List windows, tabs and panes.
    #[command(name = "list")]
    List(ListCommand),

    /// Spawn a command into a new tab or window. Prints the new pane id.
    #[command(name = "spawn", trailing_var_arg = true)]
    Spawn(SpawnCommand),

    /// Move a pane into a new tab.
    #[command(name = "move-pane-to-new-tab")]
    MovePaneToNewTab(MovePaneToNewTabCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Columns separated by spaces.
    Table,
    /// A JSON array with one object per pane.
    Json,
}

#[derive(Debug, Parser, Clone)]
struct ListCommand {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Debug, Parser, Clone)]
struct SpawnCommand {
    /// The pane whose window gets the new tab. Defaults to $WEZTERM_PANE,
    /// then to the focused pane of the most recently active client.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Spawn into this domain instead of the default domain.
    #[arg(long)]
    domain_name: Option<String>,

    /// Spawn a tab into this window instead of the pane's window.
    #[arg(long, conflicts_with_all = ["workspace", "new_window"])]
    window_id: Option<WindowId>,

    /// Spawn into a new window instead of a new tab.
    #[arg(long)]
    new_window: bool,

    /// Working directory of the spawned program. A relative path is relative
    /// to the current directory.
    #[arg(long, value_hint = ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// Workspace of the new window. Defaults to the pane's workspace.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Run PROG instead of the domain's default program, for example
    /// `spawn -- bash -l`.
    #[arg(value_hint = ValueHint::CommandWithArguments, num_args = 1..)]
    prog: Vec<OsString>,
}

#[derive(Debug, Parser, Clone)]
struct MovePaneToNewTabCommand {
    /// The pane to move. Defaults to $WEZTERM_PANE, then to the focused pane
    /// of the most recently active client.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Put the new tab in this window instead of the pane's own window.
    #[arg(long)]
    window_id: Option<WindowId>,

    /// Put the new tab in a new window.
    #[arg(long, conflicts_with = "window_id")]
    new_window: bool,

    /// Workspace of a new window. Defaults to the pane's workspace.
    #[arg(long)]
    workspace: Option<String>,
}

/// Run one `frankenterm-gui cli` command.
pub fn run(cmd: CliCommand) -> anyhow::Result<()> {
    let unix_dom = cli_unix_domain(
        std::env::var_os("FRANKENTERM_UNIX_SOCKET"),
        cmd.prefer_mux,
        cmd.class.as_deref(),
    )?;
    let mut ui = mux::connui::ConnectionUI::new_headless();
    let client = Client::new_request_only_unix_domain(&unix_dom, &mut ui).with_context(|| {
        format!(
            "connect to the FrankenTerm mux at {:?}",
            unix_dom.socket_path()
        )
    })?;
    let executor = promise::spawn::ScopedExecutor::new();
    promise::spawn::block_on(executor.run(async move {
        client
            .verify_version_compat(&ui)
            .await
            .context("negotiate the protocol with the mux")?;
        match cmd.sub {
            CliSubCommand::List(list) => run_list(&client, list).await,
            CliSubCommand::Spawn(spawn) => run_spawn(&client, spawn).await,
            CliSubCommand::MovePaneToNewTab(mv) => run_move_pane_to_new_tab(&client, mv).await,
        }
    }))
}

/// `$FRANKENTERM_UNIX_SOCKET` when set, else the default command-line
/// socket. Never one that may start a server: the client is connected with
/// auto-start off.
fn cli_unix_domain(
    frankenterm_unix_socket: Option<OsString>,
    prefer_mux: bool,
    class: Option<&str>,
) -> anyhow::Result<config::UnixDomain> {
    match frankenterm_unix_socket.filter(|path| !path.is_empty()) {
        Some(path) => Ok(config::UnixDomain {
            socket_path: Some(path.into()),
            no_serve_automatically: true,
            ..Default::default()
        }),
        None => Client::compute_unix_domain(
            prefer_mux,
            class.unwrap_or(config::gui_socket::DEFAULT_WINDOW_CLASS),
        ),
    }
}

/// One pane of a listing, with the titles of its tab and window.
#[derive(Debug, Clone)]
struct ListedPane {
    entry: PaneEntry,
    tab_title: String,
    window_title: String,
    is_floating: bool,
}

/// Every pane in a listing: the tiled panes of each tab, in tab order, then
/// the floating panes, which the listing carries separately.
fn listed_panes(listing: ListPanesResponse) -> Vec<ListedPane> {
    let ListPanesResponse {
        tabs,
        tab_titles,
        window_titles,
        floating_panes,
    } = listing;
    let window_title =
        |window_id: WindowId| window_titles.get(&window_id).cloned().unwrap_or_default();
    let mut tab_title_by_id: HashMap<TabId, String> = HashMap::new();
    let mut panes = vec![];
    for (index, tab) in tabs.into_iter().enumerate() {
        let tab_title = tab_titles.get(index).cloned().unwrap_or_default();
        let mut cursor = tab.into_tree().cursor();
        loop {
            if let Some(entry) = cursor.leaf_mut() {
                tab_title_by_id
                    .entry(entry.tab_id)
                    .or_insert_with(|| tab_title.clone());
                panes.push(ListedPane {
                    window_title: window_title(entry.window_id),
                    tab_title: tab_title.clone(),
                    entry: entry.clone(),
                    is_floating: false,
                });
            }
            match cursor.preorder_next() {
                Ok(next) => cursor = next,
                Err(_) => break,
            }
        }
    }
    for floating in floating_panes {
        let entry = floating.pane;
        panes.push(ListedPane {
            window_title: window_title(entry.window_id),
            tab_title: tab_title_by_id
                .get(&entry.tab_id)
                .cloned()
                .unwrap_or_default(),
            entry,
            is_floating: true,
        });
    }
    panes
}

fn find_pane(panes: &[ListedPane], pane_id: PaneId) -> anyhow::Result<&ListedPane> {
    panes
        .iter()
        .find(|pane| pane.entry.pane_id == pane_id)
        .ok_or_else(|| anyhow!("pane {pane_id} does not exist on this mux"))
}

/// Parse `$WEZTERM_PANE`. A value that is set but is not a pane id is an
/// error, not a reason to fall back to some other pane.
fn pane_id_from_env(value: Option<OsString>) -> anyhow::Result<Option<PaneId>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .ok_or_else(|| anyhow!("$WEZTERM_PANE is not valid UTF-8: {value:?}"))?;
    if text.is_empty() {
        return Ok(None);
    }
    text.trim()
        .parse()
        .map(Some)
        .map_err(|_| anyhow!("$WEZTERM_PANE is {text:?}, which is not a pane id"))
}

/// The focused pane of the client that most recently had input.
fn most_recently_focused_pane(clients: &[ClientInfo]) -> Option<PaneId> {
    clients
        .iter()
        .filter(|client| client.focused_pane_id.is_some())
        .max_by_key(|client| client.last_input)
        .and_then(|client| client.focused_pane_id)
}

/// The pane a command acts on or from: `--pane-id`, then `$WEZTERM_PANE`,
/// then the focused pane of the most recently active client. `None` only
/// when there is none of those; a malformed `$WEZTERM_PANE` or a failed
/// request is an error.
async fn source_pane(client: &Client, explicit: Option<PaneId>) -> anyhow::Result<Option<PaneId>> {
    if let Some(pane_id) = explicit {
        return Ok(Some(pane_id));
    }
    if let Some(pane_id) = pane_id_from_env(std::env::var_os("WEZTERM_PANE"))? {
        return Ok(Some(pane_id));
    }
    let clients = client
        .list_clients()
        .await
        .context("ask the mux which pane is focused")?
        .clients;
    Ok(most_recently_focused_pane(&clients))
}

/// [`source_pane`], for commands that cannot run without one.
async fn required_source_pane(client: &Client, explicit: Option<PaneId>) -> anyhow::Result<PaneId> {
    source_pane(client, explicit).await?.ok_or_else(|| {
        anyhow!(
            "no pane to act on: pass --pane-id, run this inside a FrankenTerm pane, \
             or focus a pane in an attached GUI"
        )
    })
}

/// `--cwd` as the absolute UTF-8 path the mux expects.
fn absolute_cwd(cwd: Option<OsString>) -> anyhow::Result<Option<String>> {
    let Some(cwd) = cwd else {
        return Ok(None);
    };
    let path = std::env::current_dir()
        .context("resolve the current directory for --cwd")?
        .join(cwd);
    let path = path
        .into_os_string()
        .into_string()
        .map_err(|path| anyhow!("--cwd {path:?} is not valid UTF-8"))?;
    Ok(Some(path))
}

async fn run_spawn(client: &Client, cmd: SpawnCommand) -> anyhow::Result<()> {
    let config = config::configuration();

    // The source pane picks the window of a new tab, and the workspace of a
    // new window. A new window does not need one.
    let (window_id, source_workspace) = match (cmd.window_id, cmd.new_window) {
        (Some(window_id), _) => (Some(window_id), None),
        (None, true) => {
            let workspace = match source_pane(client, cmd.pane_id).await? {
                Some(pane_id) => {
                    let panes = listed_panes(client.list_panes().await?);
                    Some(find_pane(&panes, pane_id)?.entry.workspace.clone())
                }
                None => None,
            };
            (None, workspace)
        }
        (None, false) => {
            let pane_id = required_source_pane(client, cmd.pane_id).await?;
            let panes = listed_panes(client.list_panes().await?);
            (Some(find_pane(&panes, pane_id)?.entry.window_id), None)
        }
    };

    let workspace = cmd
        .workspace
        .or(source_workspace)
        .or_else(|| config.default_workspace.clone())
        .unwrap_or_else(|| mux::DEFAULT_WORKSPACE.to_string());

    let spawned = client
        .spawn_v2(SpawnV2 {
            domain: cmd
                .domain_name
                .map_or(SpawnTabDomain::DefaultDomain, SpawnTabDomain::DomainName),
            window_id,
            command: if cmd.prog.is_empty() {
                None
            } else {
                Some(CommandBuilder::from_argv(cmd.prog))
            },
            command_dir: absolute_cwd(cmd.cwd)?,
            // The mux sizes a tab in an existing window to that window.
            size: config.initial_size(0, None),
            workspace,
        })
        .await?;
    println!("{}", spawned.pane_id);
    Ok(())
}

async fn run_move_pane_to_new_tab(
    client: &Client,
    cmd: MovePaneToNewTabCommand,
) -> anyhow::Result<()> {
    let pane_id = required_source_pane(client, cmd.pane_id).await?;
    let panes = listed_panes(client.list_panes().await?);
    let pane = find_pane(&panes, pane_id)?;
    let window_id = if cmd.new_window {
        None
    } else {
        // Without a window the mux would open a new one, so a pane that
        // was found always names its own.
        Some(cmd.window_id.unwrap_or(pane.entry.window_id))
    };
    client
        .move_pane_to_new_tab(MovePaneToNewTab {
            pane_id,
            window_id,
            workspace_for_new_window: cmd.workspace.or_else(|| Some(pane.entry.workspace.clone())),
        })
        .await?;
    Ok(())
}

async fn run_list(client: &Client, cmd: ListCommand) -> anyhow::Result<()> {
    let panes = listed_panes(client.list_panes().await?);
    let mut out = std::io::stdout().lock();
    match cmd.format {
        OutputFormat::Json => {
            let items: Vec<ListItem> = panes.into_iter().map(ListItem::from).collect();
            serde_json::to_writer_pretty(&mut out, &items)?;
            writeln!(out)?;
        }
        OutputFormat::Table => {
            let column = |name: &str, alignment| Column {
                name: name.to_string(),
                alignment,
            };
            let columns = [
                column("WINID", Alignment::Right),
                column("TABID", Alignment::Right),
                column("PANEID", Alignment::Right),
                column("WORKSPACE", Alignment::Left),
                column("SIZE", Alignment::Left),
                column("TITLE", Alignment::Left),
                column("CWD", Alignment::Left),
            ];
            let rows: Vec<Vec<String>> = panes.iter().map(table_row).collect();
            tabulate_output(&columns, &rows, &mut out)?;
        }
    }
    Ok(())
}

fn table_row(pane: &ListedPane) -> Vec<String> {
    let entry = &pane.entry;
    vec![
        entry.window_id.to_string(),
        entry.tab_id.to_string(),
        entry.pane_id.to_string(),
        entry.workspace.clone(),
        format!("{}x{}", entry.size.cols, entry.size.rows),
        entry.title.clone(),
        entry
            .working_dir
            .as_ref()
            .map(|url| url.as_str().to_string())
            .unwrap_or_default(),
    ]
}

#[derive(serde::Serialize)]
struct ListItemSize {
    rows: usize,
    cols: usize,
    pixel_width: usize,
    pixel_height: usize,
    dpi: u32,
}

/// One pane in `list --format json`. The fields match `wezterm cli list`,
/// plus `is_floating`; scripts parse them, so keep them stable.
#[derive(serde::Serialize)]
struct ListItem {
    window_id: WindowId,
    tab_id: TabId,
    pane_id: PaneId,
    workspace: String,
    size: ListItemSize,
    title: String,
    cwd: String,
    /// Cursor column, from the left of the pane.
    cursor_x: usize,
    /// Cursor row, from the top of the pane's visible area.
    cursor_y: usize,
    cursor_shape: termwiz::surface::CursorShape,
    cursor_visibility: termwiz::surface::CursorVisibility,
    /// Columns from the left of the tab to the left of this pane.
    left_col: usize,
    /// Rows from the top of the tab to the top of this pane.
    top_row: usize,
    tab_title: String,
    window_title: String,
    is_active: bool,
    is_zoomed: bool,
    is_floating: bool,
    tty_name: Option<String>,
}

impl From<ListedPane> for ListItem {
    fn from(pane: ListedPane) -> Self {
        let ListedPane {
            entry,
            tab_title,
            window_title,
            is_floating,
        } = pane;
        Self {
            window_id: entry.window_id,
            tab_id: entry.tab_id,
            pane_id: entry.pane_id,
            workspace: entry.workspace,
            size: ListItemSize {
                rows: entry.size.rows,
                cols: entry.size.cols,
                pixel_width: entry.size.pixel_width,
                pixel_height: entry.size.pixel_height,
                dpi: entry.size.dpi,
            },
            title: entry.title,
            cwd: entry
                .working_dir
                .map(|url| url.as_str().to_string())
                .unwrap_or_default(),
            cursor_x: entry.cursor_pos.x,
            cursor_y: usize::try_from(entry.cursor_pos.y.saturating_sub(entry.physical_top))
                .unwrap_or(0),
            cursor_shape: entry.cursor_pos.shape,
            cursor_visibility: entry.cursor_pos.visibility,
            left_col: entry.left_col,
            top_row: entry.top_row,
            tab_title,
            window_title,
            is_active: entry.is_active_pane,
            is_zoomed: entry.is_zoomed_pane,
            is_floating,
            tty_name: entry.tty_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::FloatingPaneSnapshotEntry;
    use mux::renderable::StableCursorPosition;
    use mux::tab::{FloatingPaneRect, PaneNode};
    use wezterm_term::TerminalSize;

    fn entry(window_id: WindowId, tab_id: TabId, pane_id: PaneId, workspace: &str) -> PaneEntry {
        PaneEntry {
            window_id,
            tab_id,
            pane_id,
            title: format!("pane {pane_id}"),
            size: TerminalSize {
                rows: 24,
                cols: 80,
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

    fn parse(args: &[&str]) -> Result<CliCommand, clap::Error> {
        CliCommand::try_parse_from(std::iter::once("cli").chain(args.iter().copied()))
    }

    #[test]
    fn spawn_takes_its_program_after_or_without_a_double_dash() {
        for args in [
            &["spawn", "--", "bash", "-l"][..],
            &["spawn", "bash", "-l"][..],
        ] {
            let CliSubCommand::Spawn(spawn) = parse(args).unwrap().sub else {
                panic!("{args:?} is not a spawn");
            };
            assert_eq!(
                spawn.prog,
                vec![OsString::from("bash"), OsString::from("-l")]
            );
        }
    }

    #[test]
    fn spawn_rejects_the_flag_combinations_wezterm_cli_rejects() {
        // A window id names an existing window; a workspace only applies to
        // a new one.
        assert!(parse(&["spawn", "--window-id", "1", "--new-window"]).is_err());
        assert!(parse(&["spawn", "--window-id", "1", "--workspace", "w"]).is_err());
        assert!(parse(&["spawn", "--workspace", "w"]).is_err());
        assert!(parse(&["spawn", "--new-window", "--workspace", "w"]).is_ok());
        assert!(parse(&["move-pane-to-new-tab", "--new-window", "--window-id", "2"]).is_err());
    }

    #[test]
    fn global_flags_and_list_format_parse() {
        let cmd = parse(&[
            "--prefer-mux",
            "--class",
            "work",
            "list",
            "--format",
            "json",
        ])
        .unwrap();
        assert!(cmd.prefer_mux);
        assert_eq!(cmd.class.as_deref(), Some("work"));
        let CliSubCommand::List(list) = cmd.sub else {
            panic!("not a list");
        };
        assert_eq!(list.format, OutputFormat::Json);
        let CliSubCommand::List(list) = parse(&["list"]).unwrap().sub else {
            panic!("not a list");
        };
        assert_eq!(list.format, OutputFormat::Table);
        assert!(parse(&["list", "--format", "yaml"]).is_err());
    }

    #[test]
    fn frankenterm_socket_variable_wins_and_never_auto_starts() {
        let dom = cli_unix_domain(Some(OsString::from("/run/ft/sock")), true, None).unwrap();
        assert_eq!(
            dom.socket_path,
            Some(std::path::PathBuf::from("/run/ft/sock"))
        );
        assert!(dom.no_serve_automatically);
    }

    #[test]
    fn listing_includes_floating_panes_with_their_tab_title() {
        let listing = ListPanesResponse {
            tabs: vec![
                PaneNode::Leaf(entry(1, 10, 100, "default")),
                PaneNode::Leaf(entry(2, 20, 200, "ops")),
            ],
            tab_titles: vec!["build".to_string(), "logs".to_string()],
            window_titles: HashMap::from([(1, "main".to_string())]),
            floating_panes: vec![FloatingPaneSnapshotEntry {
                pane: entry(1, 10, 101, "default"),
                rect: FloatingPaneRect {
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
        let panes = listed_panes(listing);
        let summary: Vec<_> = panes
            .iter()
            .map(|pane| {
                (
                    pane.entry.pane_id,
                    pane.tab_title.as_str(),
                    pane.window_title.as_str(),
                    pane.is_floating,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (100, "build", "main", false),
                (200, "logs", "", false),
                (101, "build", "main", true),
            ]
        );
    }

    #[test]
    fn a_pane_that_is_not_listed_is_an_error() {
        let panes = listed_panes(ListPanesResponse {
            tabs: vec![PaneNode::Leaf(entry(1, 10, 100, "default"))],
            tab_titles: vec!["t".to_string()],
            window_titles: HashMap::new(),
            floating_panes: vec![],
        });
        assert_eq!(find_pane(&panes, 100).unwrap().entry.window_id, 1);
        let err = find_pane(&panes, 999).unwrap_err().to_string();
        assert!(err.contains("pane 999"), "{err}");
    }

    #[test]
    fn wezterm_pane_must_be_a_pane_id_when_set() {
        assert_eq!(pane_id_from_env(None).unwrap(), None);
        assert_eq!(pane_id_from_env(Some(OsString::new())).unwrap(), None);
        assert_eq!(pane_id_from_env(Some("42".into())).unwrap(), Some(42));
        assert_eq!(pane_id_from_env(Some(" 7\n".into())).unwrap(), Some(7));
        let err = pane_id_from_env(Some("pane-3".into())).unwrap_err();
        assert!(err.to_string().contains("pane-3"), "{err}");
    }

    #[test]
    fn list_json_has_the_wezterm_cli_fields() {
        let mut pane = entry(3, 30, 300, "default");
        pane.cursor_pos.x = 5;
        pane.cursor_pos.y = 110;
        pane.physical_top = 100;
        let item = ListItem::from(ListedPane {
            entry: pane,
            tab_title: "tab".to_string(),
            window_title: "win".to_string(),
            is_floating: false,
        });
        let json = serde_json::to_value(&item).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "cursor_shape",
                "cursor_visibility",
                "cursor_x",
                "cursor_y",
                "cwd",
                "is_active",
                "is_floating",
                "is_zoomed",
                "left_col",
                "pane_id",
                "size",
                "tab_id",
                "tab_title",
                "title",
                "top_row",
                "tty_name",
                "window_id",
                "window_title",
                "workspace",
            ]
        );
        assert_eq!(json["cursor_x"], 5);
        assert_eq!(json["cursor_y"], 10);
        assert_eq!(json["size"]["cols"], 80);
    }
}
