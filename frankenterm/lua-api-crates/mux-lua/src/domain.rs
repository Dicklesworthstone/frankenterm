use super::*;
use mlua::UserDataRef;
use mux::domain::{Domain, DomainId, DomainState};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct MuxDomain(pub DomainId);

impl MuxDomain {
    pub fn resolve(&self, mux: &Arc<Mux>) -> mlua::Result<Arc<dyn Domain>> {
        mux.get_domain(self.0)
            .ok_or_else(|| mlua::Error::external(format!("domain id {} not found in mux", self.0)))
    }
}

impl UserData for MuxDomain {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, _: ()| {
            Ok(format!("MuxDomain(domain_id:{}, pid:{})", this.0, unsafe {
                libc::getpid()
            }))
        });
        methods.add_method("domain_id", |_, this, _: ()| Ok(this.0));

        methods.add_method("is_spawnable", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(domain.spawnable())
        });

        // Must stay an async Lua method. `promise::spawn::block_on` (the form
        // this regressed to during the mlua 0.11 migration) trips the
        // main-thread dispatch guard -> SIGABRT when `domain:attach` is invoked
        // from a `gui-startup` handler / event handler / keybinding running on
        // the main-thread spawn queue.
        //
        // We can't simply `domain.attach(..).await` directly: `Domain::attach`
        // is an `#[async_trait(?Send)]` method, so its future is `!Send` (it
        // boxes a non-Send `dyn Future`, and for remote `ClientDomain`s drives
        // network RPCs), but mlua 0.11's `add_async_method` requires a `Send`
        // future. So spawn the `!Send` work onto the main-thread queue via
        // `promise::spawn::spawn` (which uses `spawn_local`, lifting the `Send`
        // bound on the future) and await the resulting `Task` handle, which IS
        // `Send`. The event loop drives the spawned future to completion while
        // we yield here; no `block_on`, so no dispatch-guard trip, and only the
        // `Send` `Task` is live across the await, satisfying mlua.
        methods.add_async_method(
            "attach",
            |_, this, window: Option<UserDataRef<MuxWindow>>| async move {
                let mux = get_mux()?;
                let domain = this.resolve(&mux)?;
                let window_id = window.map(|w| w.0);
                promise::spawn::spawn(async move {
                    domain.attach(window_id).await.map_err(|err| {
                        mlua::Error::external(format!(
                            "failed to attach domain {}: {err:#}",
                            domain.domain_name()
                        ))
                    })
                })
                .await
            },
        );

        methods.add_method("detach", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            domain.detach().map_err(|err| {
                mlua::Error::external(format!(
                    "failed to detach domain {}: {err:#}",
                    domain.domain_name()
                ))
            })
        });

        methods.add_method("state", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(match domain.state() {
                DomainState::Attached => "Attached",
                DomainState::Detached => "Detached",
            })
        });

        methods.add_method("name", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(domain.domain_name().to_string())
        });

        // Async for the same reason as `attach`. `Domain::domain_label` is an
        // `#[async_trait(?Send)]` method (its future is `!Send`); block_on from
        // the main-thread spawn queue trips the GUI dispatch deadlock guard, and
        // mlua 0.11 requires a `Send` future. Spawn the `!Send` work onto the
        // main-thread queue and await the `Send` `Task` handle (see `attach`).
        methods.add_async_method("label", |_, this, _: ()| async move {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(promise::spawn::spawn(async move { domain.domain_label().await }).await)
        });

        methods.add_method("has_any_panes", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            let have_panes_in_domain = mux
                .iter_panes()
                .iter()
                .any(|p| p.domain_id() == domain.domain_id());
            Ok(have_panes_in_domain)
        });
    }
}
