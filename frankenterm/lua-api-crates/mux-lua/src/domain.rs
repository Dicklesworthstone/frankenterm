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

        methods.add_method(
            "attach",
            |_, this, window: Option<UserDataRef<MuxWindow>>| {
                let mux = get_mux()?;
                let domain = this.resolve(&mux)?;
                promise::spawn::block_on(domain.attach(window.map(|w| w.0))).map_err(|err| {
                    mlua::Error::external(format!(
                        "failed to attach domain {}: {err:#}",
                        domain.domain_name()
                    ))
                })
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

        methods.add_method("label", |_, this, _: ()| {
            let mux = get_mux()?;
            let domain = this.resolve(&mux)?;
            Ok(promise::spawn::block_on(domain.domain_label()))
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
