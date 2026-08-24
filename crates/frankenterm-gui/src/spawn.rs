use anyhow::{Context, anyhow, bail, ensure};
use config::TermConfig;
use config::keyassignment::SpawnCommand;
use frankenterm_toast_notification::persistent_toast_notification;
use mux::activity::Activity;
use mux::domain::SplitSource;
use mux::tab::{FloatingPaneRect, SplitRequest};
use mux::window::WindowId as MuxWindowId;
use mux::{DomainOperationGuard, Mux};
use portable_pty::CommandBuilder;
use std::sync::Arc;
use wezterm_term::TerminalSize;

#[derive(Copy, Debug, Clone, Eq, PartialEq)]
pub enum SpawnWhere {
    NewWindow,
    NewTab,
    SplitPane(SplitRequest),
    FloatingPane(FloatingPaneRect),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DomainAttachmentReceipt {
    remembered_manifest_generation: Option<u64>,
    attachment_committed: bool,
}

impl DomainAttachmentReceipt {
    #[must_use]
    pub const fn remembered_attachment(self) -> bool {
        self.remembered_manifest_generation.is_some()
    }

    #[must_use]
    pub const fn attachment_committed(self) -> bool {
        self.attachment_committed
    }
}

pub struct DomainAttachmentFailure {
    error: anyhow::Error,
    receipt: DomainAttachmentReceipt,
    stage: DomainAttachmentFailureStage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainAttachmentFailureStage {
    Preparation,
    Attach,
    PostAttachSpawn,
}

impl DomainAttachmentFailure {
    #[must_use]
    pub const fn remembered_attachment(&self) -> bool {
        self.receipt.remembered_attachment()
    }

    #[must_use]
    pub const fn establishes_domain_retry(&self) -> bool {
        matches!(self.stage, DomainAttachmentFailureStage::Attach)
            && !self.receipt.attachment_committed()
    }

    #[must_use]
    pub fn into_error(self) -> anyhow::Error {
        self.error
    }
}

pub async fn attach_domain_to_window_or_spawn_recovery(
    domain: &DomainOperationGuard,
    window_id: MuxWindowId,
    command: Option<CommandBuilder>,
    command_dir: Option<String>,
    dpi: u32,
) -> Result<DomainAttachmentReceipt, DomainAttachmentFailure> {
    let mut receipt = DomainAttachmentReceipt::default();
    let mut stage = DomainAttachmentFailureStage::Preparation;
    let result = async {
        let mux = Mux::try_get().context("mux singleton is not available")?;
        let domain_id = domain.domain_id();
        let domain_name = domain.domain_name().to_string();
        let owner_client_id = mux.active_identity();
        let lifecycle = mux_lua::reserve_domain_lifecycle(domain_name.clone())
            .context("reserving ordered domain attachment lifecycle")?
            .enter()
            .await
            .context("entering ordered domain attachment lifecycle")?;
        let remembers_attachment = domain
            .downcast_ref::<frankenterm_client::domain::ClientDomain>()
            .is_some();

        stage = DomainAttachmentFailureStage::Attach;
        let attach_result = domain
            .attach(&mux, owner_client_id, Some(window_id))
            .await
            .with_context(|| format!("attaching domain `{domain_name}` to window {window_id}"));
        if let Err(error) = attach_result {
            if remembers_attachment {
                receipt.remembered_manifest_generation = crate::remember_attached_domain_best_effort(
                    domain_name,
                    lifecycle.worker_hold(),
                )
                .await;
            }
            return Err(error);
        }
        receipt.attachment_committed = true;

        let post_attach_result = if mux.window_has_panes_in_domain(window_id, domain_id) {
            Ok(())
        } else {
            stage = DomainAttachmentFailureStage::PostAttachSpawn;
            async {
                let config = config::configuration();
                config.update_ulimit()?;
                let _tab = domain
                    .spawn(
                        &mux,
                        config.initial_size(
                            dpi,
                            Some(crate::cell_pixel_dims(&config, f64::from(dpi))?),
                        ),
                        command,
                        command_dir,
                        window_id,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "spawning recovery tab for domain `{domain_name}` in window {window_id}"
                        )
                    })?;

                ensure!(
                    mux.window_has_panes_in_domain(window_id, domain_id),
                    "domain `{domain_name}` attach/spawn completed, but window {window_id} still has no panes in that domain"
                );
                Ok(())
            }
            .await
        };

        if remembers_attachment {
            receipt.remembered_manifest_generation = crate::remember_attached_domain_best_effort(
                domain_name,
                lifecycle.worker_hold(),
            )
            .await;
        }

        post_attach_result
    }
    .await;

    if receipt.remembered_attachment() {
        // The remembered-domain set is a supervisor input snapshot. Refresh
        // it after releasing the lifecycle guard even when the immediate
        // operation failed: a successful manual attach must remain supervised
        // if its internal reconnect budget is later exhausted, while an exact
        // attach-stage failure still receives a typed handoff check at its
        // caller.
        crate::schedule_auto_connect_domains();
    }

    match result {
        Ok(()) => Ok(receipt),
        Err(error) => Err(DomainAttachmentFailure {
            error,
            receipt,
            stage,
        }),
    }
}

pub fn spawn_command_impl(
    spawn: &SpawnCommand,
    spawn_where: SpawnWhere,
    size: TerminalSize,
    src_window_id: Option<MuxWindowId>,
    term_config: Arc<TermConfig>,
) {
    let spawn = spawn.clone();

    match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        8 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
            reservation
                .spawn_local(async move {
                    if let Err(err) =
                        spawn_command_internal(spawn, spawn_where, size, src_window_id, term_config)
                            .await
                    {
                        let message = crate::bounded_gui_failure_message("Failed to spawn", &err);
                        frankenterm_gui::gui_debug_log::record(
                            log::Level::Error,
                            "frankenterm_gui::domain_spawn",
                            message.clone(),
                        );
                        log::error!("{message}");
                        persistent_toast_notification("Domain spawn failed", &message);
                    }
                })
                .detach();
        }
        rejected => {
            let message = format!(
                "main-thread scheduler rejected GUI spawn command before task construction: {rejected:?}"
            );
            log::error!("{message}");
            persistent_toast_notification("Domain spawn failed", &message);
        }
    }
}

pub async fn spawn_command_internal(
    spawn: SpawnCommand,
    spawn_where: SpawnWhere,
    size: TerminalSize,
    src_window_id: Option<MuxWindowId>,
    term_config: Arc<TermConfig>,
) -> anyhow::Result<()> {
    let mux = Mux::try_get().context("mux singleton is not available")?;
    let activity = Activity::new_for_mux(&mux);

    let current_pane_id = match src_window_id {
        Some(window_id) => {
            if let Some(tab) = mux.get_active_tab_for_window(window_id) {
                tab.get_active_pane().map(|p| p.pane_id())
            } else {
                None
            }
        }
        None => None,
    };

    let cwd = if let Some(cwd) = spawn.cwd.as_ref() {
        Some(cwd.to_str().map(|s| s.to_owned()).ok_or_else(|| {
            anyhow!(
                "Domain::spawn requires that the cwd be unicode in {:?}",
                cwd
            )
        })?)
    } else {
        None
    };

    let cmd_builder = match (
        spawn.args.as_ref(),
        spawn.cwd.as_ref(),
        spawn.set_environment_variables.is_empty(),
    ) {
        (None, None, true) => None,
        _ => {
            let mut builder = spawn
                .args
                .as_ref()
                .map(|args| CommandBuilder::from_argv(args.iter().map(Into::into).collect()))
                .unwrap_or_else(CommandBuilder::new_default_prog);
            for (k, v) in spawn.set_environment_variables.iter() {
                builder.env(k, v);
            }
            if let Some(cwd) = &spawn.cwd {
                builder.cwd(cwd);
            }
            Some(builder)
        }
    };

    let workspace = mux.active_workspace().clone();

    match spawn_where {
        SpawnWhere::SplitPane(direction) => {
            let src_window_id = match src_window_id {
                Some(id) => id,
                None => anyhow::bail!("no src window when splitting a pane?"),
            };
            if let Some(tab) = mux.get_active_tab_for_window(src_window_id) {
                let pane = tab
                    .get_active_pane()
                    .ok_or_else(|| anyhow!("tab to have a pane"))?;

                log::trace!("doing split_pane");
                let (pane, _size, _window_id, _tab_id) = mux
                    .split_pane(
                        // tab.tab_id(),
                        pane.pane_id(),
                        direction,
                        SplitSource::Spawn {
                            command: cmd_builder,
                            command_dir: cwd,
                        },
                        spawn.domain,
                        mux.active_identity(),
                    )
                    .await
                    .context("split_pane")?;
                pane.set_config(term_config);
            } else {
                bail!("there is no active tab while splitting pane!?");
            }
        }
        SpawnWhere::FloatingPane(rect) => {
            let src_window_id = match src_window_id {
                Some(id) => id,
                None => anyhow::bail!("no src window when creating a floating pane?"),
            };
            // Capture exact destination authority before the domain await.
            // The mux transaction spawns only a pane, never a temporary tab,
            // and validates the same pane generation, tab Arc, and window
            // parent together before attaching it to the floating layer.
            let target = mux.capture_floating_spawn_target(src_window_id)?;
            let receipt = mux
                .spawn_floating_pane(
                    target,
                    rect,
                    cmd_builder,
                    cwd,
                    spawn.domain,
                    term_config,
                    mux.active_identity(),
                )
                .await
                .context("spawn_floating_pane")?;
            log::info!(
                "Created floating pane {} in tab {} at ({}, {})",
                receipt.pane_id(),
                receipt.tab_id(),
                receipt.rect().left,
                receipt.rect().top,
            );
        }
        _ => {
            let (_tab, pane, window_id) = mux
                .spawn_tab_or_window(
                    match spawn_where {
                        SpawnWhere::NewWindow => None,
                        _ => src_window_id,
                    },
                    spawn.domain,
                    cmd_builder,
                    cwd,
                    size,
                    current_pane_id,
                    workspace,
                    spawn.position,
                    mux.active_identity(),
                )
                .await
                .context("spawn_tab_or_window")?;

            // If it was created in this window, it copies our handlers.
            // Otherwise, we'll pick them up when we later respond to
            // the new window being created.
            if Some(window_id) == src_window_id {
                pane.set_config(term_config);
            }
        }
    };

    drop(activity);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planted_failure(
        stage: DomainAttachmentFailureStage,
        attachment_committed: bool,
    ) -> DomainAttachmentFailure {
        DomainAttachmentFailure {
            error: anyhow::anyhow!("planted attachment failure"),
            receipt: DomainAttachmentReceipt {
                remembered_manifest_generation: Some(7),
                attachment_committed,
            },
            stage,
        }
    }

    #[test]
    fn only_uncommitted_attach_stage_failure_establishes_domain_retry() {
        assert!(
            planted_failure(DomainAttachmentFailureStage::Attach, false)
                .establishes_domain_retry()
        );
        for (stage, committed) in [
            (DomainAttachmentFailureStage::Preparation, false),
            (DomainAttachmentFailureStage::Attach, true),
            (DomainAttachmentFailureStage::PostAttachSpawn, true),
        ] {
            assert!(
                !planted_failure(stage, committed).establishes_domain_retry(),
                "stage {stage:?} committed={committed} must not promise domain recovery"
            );
        }
    }

    #[test]
    fn explicit_client_domain_attach_dials_before_best_effort_remembrance() {
        let source = include_str!("spawn.rs");
        let helper = source
            .split_once("pub async fn attach_domain_to_window_or_spawn_recovery(")
            .expect("attachment helper remains present")
            .1
            .split_once("\npub fn spawn_command_impl(")
            .expect("attachment helper remains independently bounded")
            .0;
        let attach = helper
            .find(".attach(&mux, owner_client_id, Some(window_id))")
            .expect("client-domain attachment must begin its transport");
        let remember = helper[attach..]
            .find("remember_attached_domain_best_effort(")
            .map(|offset| attach + offset)
            .expect("client-domain attachment must attempt optional remembrance");
        assert!(remember > attach);
        assert!(
            helper.contains("lifecycle.worker_hold()"),
            "the uncancellable persistence worker must retain lifecycle ordering"
        );
    }
}
