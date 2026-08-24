// The proc macros from frankenterm-dynamic-derive expand to `frankenterm_dynamic::` paths,
// but we import the crate as `wezterm_dynamic`. This alias makes both names resolve.
extern crate wezterm_dynamic as frankenterm_dynamic;

use config::keyassignment::SpawnTabDomain;
use config::lua::mlua::{self, Lua, UserData, UserDataMethods, Value as LuaValue};
use config::lua::{get_or_create_module, get_or_create_sub_module};
use luahelper::impl_lua_conversion_dynamic;
use mlua::UserDataRef;
use mux::domain::{DomainId, SplitSource};
use mux::pane::{Pane, PaneId};
use mux::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use mux::window::{Window, WindowId};
use mux::Mux;
use portable_pty::CommandBuilder;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

struct DetachOnCallerDropTask<R> {
    task: Option<promise::spawn::Task<R>>,
}

impl<R> Unpin for DetachOnCallerDropTask<R> {}

impl<R> Future for DetachOnCallerDropTask<R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let poll = Pin::new(
            this.task
                .as_mut()
                .expect("main-thread completion task was polled after completion"),
        )
        .poll(context);
        if poll.is_ready() {
            this.task.take();
        }
        poll
    }
}

impl<R> Drop for DetachOnCallerDropTask<R> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

fn admit_main_thread_completion_task<MAKE, FUT, OUTPUT>(
    service_class: promise::spawn::MainThreadServiceClass,
    operation: &'static str,
    make_future: MAKE,
) -> Result<DetachOnCallerDropTask<OUTPUT>, String>
where
    MAKE: FnOnce() -> FUT,
    FUT: std::future::Future<Output = OUTPUT> + 'static,
    OUTPUT: 'static,
{
    let reservation = match promise::spawn::try_reserve_main_thread(service_class, 8 * 1024) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => {
            return Err(format!(
                "main-thread scheduler rejected operation {operation} before task construction: {rejected:?}"
            ));
        }
    };
    let spawned = reservation.spawn_local(make_future());
    if spawned
        .initial_enqueue_receipt()
        .snapshot_after_enqueue
        .retired
    {
        drop(spawned);
        return Err(format!(
            "main-thread scheduler retired operation {operation} before its initial poll"
        ));
    }
    Ok(DetachOnCallerDropTask {
        task: Some(spawned.into_task()),
    })
}

pub(crate) async fn run_on_main_thread<MAKE, FUT, OUTPUT>(
    service_class: promise::spawn::MainThreadServiceClass,
    operation: &'static str,
    make_future: MAKE,
) -> mlua::Result<OUTPUT>
where
    MAKE: FnOnce() -> FUT,
    FUT: std::future::Future<Output = OUTPUT> + 'static,
    OUTPUT: 'static,
{
    let reservation = match promise::spawn::try_reserve_main_thread(service_class, 8 * 1024) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => {
            return Err(mlua::Error::external(format!(
                "main-thread scheduler rejected mux Lua operation {operation} before task construction: {rejected:?}"
            )));
        }
    };
    let spawned = reservation.spawn_local(make_future());
    if spawned
        .initial_enqueue_receipt()
        .snapshot_after_enqueue
        .retired
    {
        drop(spawned);
        return Err(mlua::Error::external(format!(
            "main-thread scheduler retired mux Lua operation {operation} before its initial poll"
        )));
    }
    Ok(spawned.into_task().await)
}

/// Run an admitted main-thread transaction to completion even when the Lua
/// future awaiting its result is cancelled.
///
/// This is reserved for lifecycle mutations whose request has already crossed
/// its admission boundary. Once an attach or detach starts, dropping the Lua
/// callback must not cancel the transport/persistence sequence and strand its
/// durable reconnect intent between states.
pub(crate) async fn run_on_main_thread_to_completion<MAKE, FUT, OUTPUT>(
    service_class: promise::spawn::MainThreadServiceClass,
    operation: &'static str,
    make_future: MAKE,
) -> mlua::Result<OUTPUT>
where
    MAKE: FnOnce() -> FUT,
    FUT: std::future::Future<Output = OUTPUT> + 'static,
    OUTPUT: 'static,
{
    Ok(admit_main_thread_completion_task(service_class, operation, make_future)
        .map_err(mlua::Error::external)?
        .await)
}

/// Run one owned mux transaction to completion after main-thread admission.
///
/// Unlike the Lua-facing wrapper, this preserves `anyhow` errors for the
/// mux-owned implicit-domain lifecycle hook. Dropping the waiter detaches the
/// admitted task, so transport cleanup and reconnect-intent handoff still
/// reach a terminal outcome.
pub(crate) async fn run_anyhow_on_main_thread_to_completion<MAKE, FUT, OUTPUT>(
    service_class: promise::spawn::MainThreadServiceClass,
    operation: &'static str,
    make_future: MAKE,
) -> anyhow::Result<OUTPUT>
where
    MAKE: FnOnce() -> FUT,
    FUT: std::future::Future<Output = anyhow::Result<OUTPUT>> + 'static,
    OUTPUT: 'static,
{
    admit_main_thread_completion_task(service_class, operation, make_future)
        .map_err(anyhow::Error::msg)?
        .await
}
use wezterm_dynamic::{FromDynamic, ToDynamic};
use wezterm_term::TerminalSize;

mod domain;
mod pane;
mod tab;
mod window;

pub use domain::MuxDomain;
pub use domain::{
    DomainLifecycleEvent, DomainLifecycleGuard, DomainLifecycleWorkerHold,
    install_domain_lifecycle_recorder, reserve_domain_lifecycle,
};
pub use pane::MuxPane;
pub use tab::MuxTab;
pub use window::MuxWindow;

fn get_mux() -> mlua::Result<Arc<Mux>> {
    Mux::try_get().ok_or_else(|| mlua::Error::external("cannot get Mux!?"))
}

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let mux_mod = get_or_create_sub_module(lua, "mux")?;

    mux_mod.set(
        "get_active_workspace",
        lua.create_function(|_, _: ()| {
            let mux = get_mux()?;
            Ok(mux.active_workspace())
        })?,
    )?;

    mux_mod.set(
        "get_workspace_names",
        lua.create_function(|_, _: ()| {
            let mux = get_mux()?;
            Ok(mux.iter_workspaces())
        })?,
    )?;

    mux_mod.set(
        "set_active_workspace",
        lua.create_function(|_, workspace: String| {
            let mux = get_mux()?;
            let workspaces = mux.iter_workspaces();
            if workspaces.contains(&workspace) {
                let _: () = mux.set_active_workspace(&workspace);
                Ok(())
            } else {
                Err(mlua::Error::external(format!(
                    "{:?} is not an existing workspace",
                    workspace
                )))
            }
        })?,
    )?;

    mux_mod.set(
        "rename_workspace",
        lua.create_function(|_, (old_workspace, new_workspace): (String, String)| {
            let mux = get_mux()?;
            mux.rename_workspace(&old_workspace, &new_workspace)
                .map_err(mlua::Error::external)?;
            Ok(())
        })?,
    )?;

    mux_mod.set(
        "get_window",
        lua.create_function(|_, window_id: WindowId| {
            let mux = get_mux()?;
            let window = MuxWindow(window_id);
            let _resolved = window.resolve(&mux)?;
            Ok(window)
        })?,
    )?;

    mux_mod.set(
        "get_pane",
        lua.create_function(|_, pane_id: PaneId| {
            let mux = get_mux()?;
            let pane = MuxPane(pane_id);
            pane.resolve(&mux)?;
            Ok(pane)
        })?,
    )?;

    mux_mod.set(
        "get_tab",
        lua.create_function(|_, tab_id: TabId| {
            let mux = get_mux()?;
            let tab = MuxTab(tab_id);
            tab.resolve(&mux)?;
            Ok(tab)
        })?,
    )?;

    mux_mod.set(
        "spawn_window",
        // Must stay an async Lua function. `promise::spawn::block_on` (the form
        // this regressed to during the mlua 0.11 migration) trips the
        // main-thread dispatch guard -> SIGABRT when `mux.spawn_window` is
        // invoked from a `gui-startup` handler / event handler / keybinding
        // running on the main-thread spawn queue.
        //
        // We can't simply `spawn.spawn().await` directly: it awaits
        // `mux.spawn_tab_or_window`, whose future is `!Send` (it boxes non-Send
        // `dyn Future`s, and for remote `ClientDomain`s drives network RPCs),
        // but mlua 0.11's `create_async_function` requires a `Send` future.
        //
        // So spawn the `!Send` work onto the main-thread queue via
        // `promise::spawn::spawn` (which uses `spawn_local`, lifting the `Send`
        // bound on the future) and await the resulting `Task` handle, which IS
        // `Send`. The event loop drives the spawned future to completion --
        // including remote-domain I/O -- while we yield here; no `block_on`, so
        // no dispatch-guard trip, and only the `Send` `Task` is live across the
        // await, satisfying mlua.
        lua.create_async_function(|_, spawn: SpawnWindow| async move {
            run_on_main_thread(
                promise::spawn::MainThreadServiceClass::Topology,
                "spawn window",
                || spawn.spawn(),
            )
            .await?
        })?,
    )?;

    mux_mod.set(
        "all_windows",
        lua.create_function(|_, _: ()| {
            let mux = get_mux()?;
            Ok(mux
                .iter_windows()
                .into_iter()
                .map(MuxWindow)
                .collect::<Vec<MuxWindow>>())
        })?,
    )?;

    mux_mod.set(
        "get_domain",
        lua.create_function(|_, domain: LuaValue| {
            let mux = get_mux()?;
            match domain {
                LuaValue::Nil => mux
                    .default_domain()
                    .map(|domain| Some(MuxDomain(domain.domain_id())))
                    .map_err(mlua::Error::external),
                LuaValue::String(s) => match s.to_str() {
                    Ok(name) => Ok(mux
                        .get_domain_by_name(&name)
                        .map(|dom| MuxDomain(dom.domain_id()))),
                    Err(err) => Err(mlua::Error::external(format!(
                        "invalid domain identifier passed to mux.get_domain: {err:#}"
                    ))),
                },
                LuaValue::Integer(id) => match TryInto::<DomainId>::try_into(id) {
                    Ok(id) => Ok(mux.get_domain(id).map(|dom| MuxDomain(dom.domain_id()))),
                    Err(err) => Err(mlua::Error::external(format!(
                        "invalid domain identifier passed to mux.get_domain: {err:#}"
                    ))),
                },
                _ => Err(mlua::Error::external(
                    "invalid domain identifier passed to mux.get_domain".to_string(),
                )),
            }
        })?,
    )?;

    mux_mod.set(
        "all_domains",
        lua.create_function(|_, _: ()| {
            let mux = get_mux()?;
            Ok(mux
                .iter_domains()
                .into_iter()
                .map(|dom| MuxDomain(dom.domain_id()))
                .collect::<Vec<MuxDomain>>())
        })?,
    )?;

    mux_mod.set(
        "set_default_domain",
        lua.create_function(|_, domain: UserDataRef<MuxDomain>| {
            let mux = get_mux()?;
            let domain = domain.resolve(&mux)?;
            mux.set_default_domain_guard(&domain)
                .map_err(mlua::Error::external)?;
            Ok(())
        })?,
    )?;

    Ok(())
}

#[derive(Debug, Default, FromDynamic, ToDynamic)]
struct CommandBuilderFrag {
    args: Option<Vec<String>>,
    cwd: Option<String>,
    #[dynamic(default)]
    set_environment_variables: HashMap<String, String>,
}

impl CommandBuilderFrag {
    fn to_command_builder(&self) -> (Option<CommandBuilder>, Option<String>) {
        if let Some(args) = &self.args {
            let mut builder = CommandBuilder::from_argv(args.iter().map(Into::into).collect());
            for (k, v) in self.set_environment_variables.iter() {
                builder.env(k, v);
            }
            if let Some(cwd) = self.cwd.clone() {
                builder.cwd(cwd);
            }
            (Some(builder), None)
        } else {
            (None, self.cwd.clone())
        }
    }
}

#[derive(Debug, FromDynamic, ToDynamic, Default)]
enum HandySplitDirection {
    Left,
    #[default]
    Right,
    Top,
    Bottom,
}
impl_lua_conversion_dynamic!(HandySplitDirection);

#[derive(Debug, FromDynamic, ToDynamic)]
struct SpawnWindow {
    #[dynamic(default = "spawn_tab_default_domain")]
    domain: SpawnTabDomain,
    width: Option<usize>,
    height: Option<usize>,
    workspace: Option<String>,
    position: Option<config::GuiPosition>,
    #[dynamic(flatten)]
    cmd_builder: CommandBuilderFrag,
}
impl_lua_conversion_dynamic!(SpawnWindow);

fn spawn_tab_default_domain() -> SpawnTabDomain {
    SpawnTabDomain::DefaultDomain
}

impl SpawnWindow {
    async fn spawn(self) -> mlua::Result<(MuxTab, MuxPane, MuxWindow)> {
        let mux = get_mux()?;

        let size = match (self.width, self.height) {
            (Some(cols), Some(rows)) => TerminalSize {
                rows,
                cols,
                ..Default::default()
            },
            _ => config::configuration().initial_size(0, None),
        };

        let (cmd_builder, cwd) = self.cmd_builder.to_command_builder();
        let (tab, pane, window_id) = mux
            .spawn_tab_or_window(
                None,
                self.domain,
                cmd_builder,
                cwd,
                size,
                None,
                self.workspace.unwrap_or_else(|| mux.active_workspace()),
                self.position,
                mux.active_identity(),
            )
            .await
            .map_err(|e| mlua::Error::external(format!("{:#?}", e)))?;

        Ok((
            MuxTab(tab.tab_id()),
            MuxPane(pane.pane_id()),
            MuxWindow(window_id),
        ))
    }
}

#[derive(Debug, FromDynamic, ToDynamic)]
struct SpawnTab {
    #[dynamic(default)]
    domain: SpawnTabDomain,
    #[dynamic(flatten)]
    cmd_builder: CommandBuilderFrag,
}
impl_lua_conversion_dynamic!(SpawnTab);

impl SpawnTab {
    async fn spawn(self, window: &MuxWindow) -> mlua::Result<(MuxTab, MuxPane, MuxWindow)> {
        let mux = get_mux()?;
        let size;
        let pane;

        {
            let window = window.resolve(&mux)?;
            size = window
                .get_by_idx(0)
                .map(|tab| tab.get_size())
                .unwrap_or_else(|| config::configuration().initial_size(0, None));

            pane = window
                .get_active()
                .and_then(|tab| tab.get_active_pane().map(|pane| pane.pane_id()));
        };

        let (cmd_builder, cwd) = self.cmd_builder.to_command_builder();

        let (tab, pane, window_id) = mux
            .spawn_tab_or_window(
                Some(window.0),
                self.domain,
                cmd_builder,
                cwd,
                size,
                pane,
                String::new(),
                None, // optional gui window position
                mux.active_identity(),
            )
            .await
            .map_err(|e| mlua::Error::external(format!("{:#?}", e)))?;

        Ok((
            MuxTab(tab.tab_id()),
            MuxPane(pane.pane_id()),
            MuxWindow(window_id),
        ))
    }
}

#[derive(Clone, FromDynamic, ToDynamic)]
struct MuxTabInfo {
    pub index: usize,
    pub is_active: bool,
}
impl_lua_conversion_dynamic!(MuxTabInfo);

#[derive(Clone, FromDynamic, ToDynamic)]
struct MuxPaneInfo {
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
}
impl_lua_conversion_dynamic!(MuxPaneInfo);
