#![allow(clippy::future_not_send)]
use crate::PKI;
use anyhow::{Context, anyhow};
use codec::{
    ActivatePaneDirection, AdjustPaneSize, CODEC_VERSION, CreateFloatingPane, CycleStack,
    DecodedPdu, EraseScrollbackRequest, ErrorResponse, GetClientList, GetClientListResponse,
    GetCodecVersionResponse, GetImageCell, GetImageCellResponse, GetLines, GetLinesResponse,
    GetPaneDirection, GetPaneDirectionResponse, GetPaneRenderChanges, GetPaneRenderChangesResponse,
    GetPaneRenderableDimensions, GetPaneRenderableDimensionsResponse, GetTlsCredsResponse,
    InputSerial, KillPane, ListPanes, ListPanesResponse, LivenessResponse, MoveFloatingPane,
    MovePaneToNewTab, MovePaneToNewTabResponse, NotifyAlert, Pdu, Ping, Pong, RemoveFloatingPane,
    RenameWorkspace, Resize, SearchScrollbackRequest, SearchScrollbackResponse, SelectStackPane,
    SendKeyDown, SendKeyUp, SendMouseEvent, SendPaste, SetActiveWorkspace, SetClientId,
    SetFloatingPaneZ, SetFocusedPane, SetLayoutCycle, SetPalette, SetPaneZoomed,
    SetWindowWorkspace, SpawnResponse, SpawnV2, SplitPane, SwapToLayout, TabTitleChanged,
    ToggleFloatingPane, UnitResponse, UpdatePaneConstraints, WindowTitleChanged, WriteToPane,
};
use config::TermConfig;
use mux::client::ClientId;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::{Mux, MuxNotification};
use promise::spawn::spawn_into_main_thread;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_term::StableRowIndex;
use wezterm_term::terminal::Alert;

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu)
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
}

#[derive(Default, Debug)]
pub(crate) struct PerPane {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
    mouse_grabbed: bool,
    alt_screen_active: bool,
    sent_initial_palette: bool,
    seqno: SequenceNo,
    config_generation: usize,
    pub(crate) notifications: Vec<Alert>,
}

impl PerPane {
    fn compute_changes(
        &mut self,
        pane: &Arc<dyn Pane>,
        force_with_input_serial: Option<InputSerial>,
    ) -> Option<GetPaneRenderChangesResponse> {
        let mut changed = false;
        let mouse_grabbed = pane.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }
        let alt_screen_active = pane.is_alt_screen_active();
        if alt_screen_active != self.alt_screen_active {
            changed = true;
        }

        let dims = pane.get_dimensions();
        if dims != self.dimensions {
            changed = true;
        }
        let tiered_scrollback_status = pane.get_tiered_scrollback_status();
        if tiered_scrollback_status != self.tiered_scrollback_status {
            changed = true;
        }

        let cursor_position = pane.get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = pane.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        if working_dir != self.working_dir {
            changed = true;
        }

        let old_seqno = self.seqno;
        self.seqno = pane.get_current_seqno();
        let mut all_dirty_lines = pane.get_changed_since(
            0..dims.physical_top + dims.viewport_rows as StableRowIndex,
            old_seqno,
        );
        if !all_dirty_lines.is_empty() {
            changed = true;
        }

        if !changed && !force_with_input_serial.is_some() {
            return None;
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let viewport_range =
            dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;

        let (first_line, lines) = pane.get_lines(viewport_range);
        let mut bonus_lines = lines
            .into_iter()
            .enumerate()
            .filter_map(|(idx, mut line)| {
                let stable_row = first_line + idx as StableRowIndex;
                if all_dirty_lines.contains(stable_row) {
                    all_dirty_lines.remove(stable_row);
                    line.compress_for_scrollback();
                    Some((stable_row, line))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let (cursor_line_idx, lines) =
            pane.get_lines(cursor_position.y..cursor_position.y.saturating_add(1));
        if let Some(mut cursor_line) = lines.into_iter().next() {
            cursor_line.compress_for_scrollback();
            bonus_lines.push((cursor_line_idx, cursor_line));
        }

        self.cursor_position = cursor_position;
        self.title.clone_from(&title);
        self.working_dir.clone_from(&working_dir);
        self.dimensions = dims;
        self.tiered_scrollback_status = tiered_scrollback_status;
        self.mouse_grabbed = mouse_grabbed;
        self.alt_screen_active = alt_screen_active;

        let bonus_lines = bonus_lines.into();
        Some(GetPaneRenderChangesResponse {
            pane_id: pane.pane_id(),
            mouse_grabbed,
            alt_screen_active,
            dirty_lines: all_dirty_lines.iter().cloned().collect(),
            dimensions: dims,
            tiered_scrollback_status,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial: force_with_input_serial,
            seqno: self.seqno,
        })
    }
}

fn maybe_push_pane_changes(
    pane: &Arc<dyn Pane>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    let mut per_pane = per_pane
        .lock()
        .map_err(|err| anyhow!("per-pane state lock poisoned: {err}"))?;
    if let Some(resp) = per_pane.compute_changes(pane, None) {
        sender.send(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        })?;
    }

    let config = config::configuration();
    if per_pane.config_generation != config.generation() {
        per_pane.config_generation = config.generation();
        // If the config changed, it may have changed colors
        // in the palette that we need to push down, so we
        // synthesize a palette change notification to let
        // the client know
        per_pane.notifications.push(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }

    if !per_pane.sent_initial_palette {
        per_pane.notifications.push(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }
    for alert in per_pane.notifications.drain(..) {
        match alert {
            Alert::PaletteChanged => {
                sender.send(DecodedPdu {
                    pdu: Pdu::SetPalette(SetPalette {
                        pane_id: pane.pane_id(),
                        palette: pane.palette(),
                    }),
                    serial: 0,
                })?;
            }
            alert => {
                sender.send(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    serial: 0,
                })?;
            }
        }
    }
    Ok(())
}

fn find_tab_matching<F>(mux: &Mux, mut predicate: F) -> Option<Arc<mux::tab::Tab>>
where
    F: FnMut(&Arc<mux::tab::Tab>) -> bool,
{
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        for tab in window.iter() {
            if predicate(tab) {
                return Some(Arc::clone(tab));
            }
        }
    }
    None
}

fn find_tab_with_floating_pane(mux: &Mux, pane_id: PaneId) -> Option<Arc<mux::tab::Tab>> {
    find_tab_matching(mux, |tab| tab.has_floating_pane(pane_id))
}

fn find_tab_containing_pane(mux: &Mux, pane_id: PaneId) -> Option<Arc<mux::tab::Tab>> {
    find_tab_matching(mux, |tab| tab.contains_pane(pane_id))
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    per_pane: HashMap<PaneId, Arc<Mutex<PerPane>>>,
    client_id: Option<Arc<ClientId>>,
    proxy_client_id: Option<ClientId>,
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        if let Some(client_id) = self.client_id.take() {
            if let Some(mux) = Mux::try_get() {
                mux.unregister_client(&client_id);
            }
        }
    }
}

impl SessionHandler {
    pub fn new(to_write_tx: PduSender) -> Self {
        Self {
            to_write_tx,
            per_pane: HashMap::new(),
            client_id: None,
            proxy_client_id: None,
        }
    }

    pub(crate) fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        Arc::clone(
            self.per_pane
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerPane::default()))),
        )
    }

    /// Non-inserting accessor for cached per-pane state (ft-12e8l).
    ///
    /// Unlike [`per_pane`], this does NOT create an entry when the pane is
    /// untracked. Use this on push-side paths (e.g. late-arriving Alert
    /// notifications from mux) where silently re-creating an entry for a
    /// pane that was already removed via `PaneRemoved` produces a permanent
    /// map leak — no subsequent `PaneRemoved` ever fires for a dead pane.
    pub(crate) fn per_pane_if_present(
        &self,
        pane_id: PaneId,
    ) -> Option<Arc<Mutex<PerPane>>> {
        self.per_pane.get(&pane_id).map(Arc::clone)
    }

    /// Remove cached per-pane state when a pane is destroyed.
    /// Prevents unbounded HashMap growth in long-lived sessions.
    pub(crate) fn remove_per_pane(&mut self, pane_id: PaneId) {
        self.per_pane.remove(&pane_id);
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane(pane_id);
        spawn_into_main_thread(async move {
            let mux = Mux::get();
            let pane = mux
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
            maybe_push_pane_changes(&pane, sender, per_pane)?;
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;

        if let Some(client_id) = &self.client_id {
            if decoded.pdu.is_user_input() {
                Mux::get().client_had_input(client_id);
            }
        }

        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            sender.send(DecodedPdu { pdu, serial }).ok();
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        match decoded.pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                            window.set_workspace(&workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetActiveWorkspace(SetActiveWorkspace { workspace }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("set active workspace before SetClientId")
                            })?;
                            let mux = Mux::get();
                            mux.set_active_workspace_for_client(&client_id, &workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetClientId(SetClientId {
                mut client_id,
                is_proxy,
            }) => {
                if is_proxy {
                    if self.proxy_client_id.is_none() {
                        // Copy proxy identity, but don't assign it to the mux;
                        // we'll use it to annotate the actual clients own
                        // identity when they send it
                        self.proxy_client_id.replace(client_id);
                    }
                } else {
                    // If this session is a proxy, override the incoming id with
                    // the proxy information so that it is clear what is going
                    // on from the `wezterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock.clone_from(&proxy_id.ssh_auth_sock);
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }

                    let client_id = Arc::new(client_id);
                    if self.client_id.as_ref() != Some(&client_id) {
                        let prior_client_id = self.client_id.replace(client_id.clone());
                        let mux = Mux::get();
                        if let Some(prior_client_id) = prior_client_id {
                            mux.unregister_client(&prior_client_id);
                        }
                        mux.register_client(client_id);
                    }
                }
                send_response(Ok(Pdu::UnitResponse(UnitResponse {})));
            }
            Pdu::SetFocusedPane(SetFocusedPane { pane_id }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

                            let (_domain_id, window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            {
                                let mut window =
                                    mux.get_window_mut(window_id).ok_or_else(|| {
                                        anyhow::anyhow!("window {window_id} not found")
                                    })?;
                                let tab_idx = window.idx_by_id(tab_id).ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "tab {tab_id} isn't really in window {window_id}!?"
                                    )
                                })?;
                                window.save_and_then_set_active(tab_idx);
                            }
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;
                            tab.set_active_pane(&pane);

                            mux.record_focus_for_current_identity(pane_id);
                            mux.notify(mux::MuxNotification::PaneFocused(pane_id));

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::GetClientList(GetClientList) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let clients = mux.iter_clients();
                            Ok(Pdu::GetClientListResponse(GetClientListResponse {
                                clients,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanes(ListPanes {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut tabs = vec![];
                            let mut tab_titles = vec![];
                            let mut window_titles = HashMap::new();
                            for window_id in mux.iter_windows() {
                                let Some(window) = mux.get_window(window_id) else {
                                    log::warn!(
                                        "ListPanes skipped stale window id {} from iter_windows",
                                        window_id
                                    );
                                    continue;
                                };
                                window_titles.insert(window_id, window.get_title().to_string());
                                for tab in window.iter() {
                                    tabs.push(tab.codec_pane_tree());
                                    tab_titles.push(tab.get_title());
                                }
                            }
                            log::trace!("ListPanes {tabs:#?} {tab_titles:?}");
                            Ok(Pdu::ListPanesResponse(ListPanesResponse {
                                tabs,
                                tab_titles,
                                window_titles,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            mux.rename_workspace(&old_workspace, &new_workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.writer().write_all(&data)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.erase_scrollback(erase_mode);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::KillPane(KillPane { pane_id }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                // Clean up cached per-pane state to avoid unbounded growth.
                self.per_pane.remove(&pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.kill();
                            mux.remove_pane(pane_id);
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.send_paste(&data)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                use mux::pane::Pattern;

                async fn do_search(
                    pane_id: PaneId,
                    pattern: Pattern,
                    range: std::ops::Range<StableRowIndex>,
                    limit: Option<u32>,
                ) -> anyhow::Result<Pdu> {
                    let mux = Mux::get();
                    let pane = mux
                        .get_pane(pane_id)
                        .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                    pane.search(pattern, range, limit).await.map(|results| {
                        Pdu::SearchScrollbackResponse(SearchScrollbackResponse { results })
                    })
                }

                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = do_search(pane_id, pattern, range, limit).await;
                        send_response(result);
                    })
                    .detach();
                })
                .detach();
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            match tab.get_zoomed_pane() {
                                Some(p) => {
                                    let is_zoomed = p.pane_id() == pane_id;
                                    if is_zoomed != zoomed {
                                        tab.set_zoomed(false);
                                        if zoomed {
                                            tab.set_active_pane(&pane);
                                            tab.set_zoomed(zoomed);
                                        }
                                    }
                                }
                                None => {
                                    if zoomed {
                                        tab.set_active_pane(&pane);
                                        tab.set_zoomed(zoomed);
                                    }
                                }
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let panes = tab.iter_panes_ignoring_zoom();
                            let pane_id = tab
                                .get_pane_direction(direction, true)
                                .map(|pane_index| panes[pane_index].pane.pane_id());

                            Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                pane_id,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.activate_pane_direction(direction);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.resize(size)?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            tab.rebuild_splits_sizes_from_contained_panes();
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.key_down(event.key, event.modifiers)?;

                            // For a key press, we want to always send back the
                            // cursor position so that the predictive echo doesn't
                            // leave the cursor in the wrong place
                            let mut per_pane = per_pane
                                .lock()
                                .map_err(|err| anyhow!("per-pane state lock poisoned: {err}"))?;
                            if let Some(resp) = per_pane.compute_changes(&pane, Some(input_serial))
                            {
                                sender.send(DecodedPdu {
                                    pdu: Pdu::GetPaneRenderChangesResponse(resp),
                                    serial: 0,
                                })?;
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendKeyUp(SendKeyUp { pane_id, event }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.key_up(event.key, event.modifiers)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.mouse_event(event)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SpawnV2(spawn) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_domain_spawn_v2(spawn, send_response, client_id);
                })
                .detach();
            }

            Pdu::SplitPane(split) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_split_pane(split, send_response, client_id);
                })
                .detach();
            }

            Pdu::MovePaneToNewTab(request) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_move_pane(request, send_response, client_id);
                })
                .detach();
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let cursor_position = pane.get_cursor_position();
                            let dimensions = pane.get_dimensions();
                            Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                GetPaneRenderableDimensionsResponse {
                                    pane_id,
                                    cursor_position,
                                    dimensions,
                                    tiered_scrollback_status: pane.get_tiered_scrollback_status(),
                                },
                            ))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let is_alive = match mux.get_pane(pane_id) {
                                Some(pane) => {
                                    maybe_push_pane_changes(&pane, sender, per_pane)?;
                                    true
                                }
                                None => false,
                            };
                            Ok(Pdu::LivenessResponse(LivenessResponse {
                                pane_id,
                                is_alive,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let mut lines_and_indices = vec![];

                            for range in lines {
                                let (first_row, lines) = pane.get_lines(range);
                                for (idx, mut line) in lines.into_iter().enumerate() {
                                    let stable_row = first_row + idx as StableRowIndex;
                                    line.compress_for_scrollback();
                                    lines_and_indices.push((stable_row, line));
                                }
                            }
                            Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                pane_id,
                                lines: lines_and_indices.into(),
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut data = None;

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                            let (_, lines) = pane.get_lines(line_idx..line_idx + 1);
                            'found_data: for line in lines {
                                if let Some(cell) = line.get_cell(cell_idx) {
                                    if let Some(images) = cell.attrs().images() {
                                        for im in images {
                                            if im.image_data().hash() == data_hash {
                                                data.replace(im.image_data().clone());
                                                break 'found_data;
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                                pane_id,
                                data,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetCodecVersion(_) => {
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
                                .map(Into::into),
                        })));
                    }
                }
            }

            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {window_id}"))?;

                            window.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;

                            tab.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                            match pane.get_config() {
                                Some(config) => match config.downcast_ref::<TermConfig>() {
                                    Some(tc) => tc.set_client_palette(palette),
                                    None => {
                                        log::error!(
                                            "pane {pane_id} doesn't \
                                            have TermConfig as its config! \
                                            Ignoring client palette update"
                                        );
                                    }
                                },
                                None => {
                                    let config = TermConfig::new();
                                    config.set_client_palette(palette);
                                    pane.set_config(Arc::new(config));
                                }
                            }

                            mux.notify(MuxNotification::Alert {
                                pane_id,
                                alert: Alert::PaletteChanged,
                            });

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_pane_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

                            let tab = match mux.get_tab(tab_id) {
                                Some(tab) => tab,
                                None => {
                                    return Err(anyhow!(
                                        "Failed to retrieve tab with ID {}",
                                        tab_id
                                    ));
                                }
                            };

                            tab.adjust_pane_size(direction, amount);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id,
                pane_id,
                rect,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                if tab.has_floating_pane(pane_id) {
                                    tab.set_floating_pane_rect(pane_id, rect);
                                    tab.set_floating_pane_focus(pane_id);
                                    return Ok(Pdu::UnitResponse(UnitResponse {}));
                                }
                                if tab.contains_pane(pane_id) {
                                    return Err(anyhow!(
                                        "pane {} is already tiled in tab {}; floating create expects a detached pane",
                                        pane_id,
                                        tab_id
                                    ));
                                }
                                let pane = mux
                                    .get_pane(pane_id)
                                    .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                                tab.add_floating_pane(pane, rect);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::MoveFloatingPane(MoveFloatingPane { pane_id, rect }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = find_tab_with_floating_pane(&mux, pane_id).ok_or_else(
                                    || anyhow!("floating pane {} not found", pane_id),
                                )?;
                                tab.set_floating_pane_rect(pane_id, rect).ok_or_else(|| {
                                    anyhow!("floating pane {} not found", pane_id)
                                })?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SetFloatingPaneZ(SetFloatingPaneZ { pane_id, z_order }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = find_tab_with_floating_pane(&mux, pane_id).ok_or_else(
                                    || anyhow!("floating pane {} not found", pane_id),
                                )?;
                                if !tab.set_floating_pane_z_order(pane_id, z_order) {
                                    return Err(anyhow!("floating pane {} not found", pane_id));
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::ToggleFloatingPane(ToggleFloatingPane { pane_id, visible }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = find_tab_with_floating_pane(&mux, pane_id).ok_or_else(
                                    || anyhow!("floating pane {} not found", pane_id),
                                )?;
                                if !tab.set_floating_pane_visible(pane_id, visible) {
                                    return Err(anyhow!("floating pane {} not found", pane_id));
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = find_tab_with_floating_pane(&mux, pane_id).ok_or_else(
                                    || anyhow!("floating pane {} not found", pane_id),
                                )?;
                                tab.remove_floating_pane(pane_id).ok_or_else(|| {
                                    anyhow!("floating pane {} not found", pane_id)
                                })?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SwapToLayout(SwapToLayout {
                tab_id,
                layout_index,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                tab.swap_to_layout_index(layout_index);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SetLayoutCycle(SetLayoutCycle {
                tab_id,
                layout_names,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                // Build layout cycle from named presets
                                let mut layouts = Vec::new();
                                for name in &layout_names {
                                    let layout = match name.as_str() {
                                        "grid-4" => mux::layout::grid_4(),
                                        "main-side" => mux::layout::main_side(),
                                        "stacked" => mux::layout::stacked(),
                                        "main-bottom" => mux::layout::main_bottom(),
                                        other => {
                                            return Err(anyhow!("unknown layout preset: {other}"));
                                        }
                                    };
                                    layouts.push(layout);
                                }
                                if layouts.is_empty() {
                                    return Err(anyhow!(
                                        "layout cycle must have at least one layout"
                                    ));
                                }
                                tab.set_layout_cycle(mux::layout::LayoutCycle::new(layouts));
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::CycleStack(CycleStack {
                tab_id,
                slot_index,
                forward,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                if forward {
                                    tab.cycle_stack(slot_index);
                                } else {
                                    tab.cycle_stack_backward(slot_index);
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SelectStackPane(SelectStackPane {
                tab_id,
                slot_index,
                pane_index,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                tab.select_stack_pane(slot_index, pane_index)
                                    .ok_or_else(|| {
                                        anyhow!(
                                            "stack slot {} or pane index {} not found in tab {}",
                                            slot_index,
                                            pane_index,
                                            tab_id
                                        )
                                    })?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id,
                min_width,
                max_width,
                min_height,
                max_height,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = Mux::get();
                                let tab =
                                    find_tab_containing_pane(&mux, pane_id).ok_or_else(|| {
                                        anyhow!("pane {} not found in any tab", pane_id)
                                    })?;
                                tab.update_pane_constraints(
                                    pane_id, min_width, max_width, min_height, max_height,
                                )
                                .ok_or_else(|| anyhow!("pane {} not found in tab", pane_id))?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)));
            }
        }
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn_v2<SND>(
    spawn: SpawnV2,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(domain_spawn_v2(spawn, client_id).await) })
        .detach();
}

fn schedule_split_pane<SND>(split: SplitPane, send_response: SND, client_id: Option<Arc<ClientId>>)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(split_pane(split, client_id).await) })
        .detach();
}

async fn split_pane(split: SplitPane, client_id: Option<Arc<ClientId>>) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(client_id);

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(split.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", split.pane_id))?;

    let source = if let Some(move_pane_id) = split.move_pane_id {
        SplitSource::MovePane(move_pane_id)
    } else {
        SplitSource::Spawn {
            command: split.command,
            command_dir: split.command_dir,
        }
    };

    let (pane, size) = mux
        .split_pane(split.pane_id, split.split_request, source, split.domain)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn domain_spawn_v2(spawn: SpawnV2, client_id: Option<Arc<ClientId>>) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(client_id);

    let (tab, pane, window_id) = mux
        .spawn_tab_or_window(
            spawn.window_id,
            spawn.domain,
            spawn.command,
            spawn.command_dir,
            spawn.size,
            None, // optional current pane_id
            spawn.workspace,
            None, // optional gui window position
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

fn schedule_move_pane<SND>(
    request: MovePaneToNewTab,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(move_pane(request, client_id).await) })
        .detach();
}

async fn move_pane(
    request: MovePaneToNewTab,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(client_id);

    let (tab, window_id) = mux
        .move_pane_to_new_tab(
            request.pane_id,
            request.window_id,
            request.workspace_for_new_window,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id: tab.tab_id(),
        window_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux::domain::DomainId;
    use mux::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, WithPaneLines};
    use parking_lot::MappedMutexGuard;
    use promise::spawn::SimpleExecutor;
    use rangeset::RangeSet;
    use std::ops::Range;
    use std::sync::Mutex as StdMutex;
    use termwiz::surface::Line;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    // These tests mutate process-global mux/executor state, so they must not run in parallel.
    static GLOBAL_STATE_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    struct ScopedMux(Option<Arc<Mux>>);

    impl ScopedMux {
        fn install(mux: &Arc<Mux>) -> Self {
            let prior = Mux::try_get();
            Mux::set_mux(mux);
            Self(prior)
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.0.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    #[derive(Clone)]
    struct FakePaneState {
        cursor_position: StableCursorPosition,
        dimensions: RenderableDimensions,
        tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
        title: String,
        working_dir: Option<Url>,
        alt_screen_active: bool,
        seqno: SequenceNo,
        lines: Vec<Line>,
    }

    struct FakePane {
        pane_id: PaneId,
        state: Mutex<FakePaneState>,
    }

    impl FakePane {
        fn new(tiered_scrollback_status: Option<PaneTieredScrollbackStatus>) -> Self {
            Self::new_with_id(77, tiered_scrollback_status)
        }

        fn new_with_id(
            pane_id: PaneId,
            tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
        ) -> Self {
            Self {
                pane_id,
                state: Mutex::new(FakePaneState {
                    cursor_position: StableCursorPosition {
                        x: 4,
                        y: 0,
                        ..Default::default()
                    },
                    dimensions: RenderableDimensions {
                        cols: 80,
                        viewport_rows: 2,
                        scrollback_rows: 12,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: 96,
                        pixel_width: 640,
                        pixel_height: 480,
                        reverse_video: false,
                    },
                    tiered_scrollback_status,
                    title: "tiered-pane".to_string(),
                    working_dir: Url::parse("file:///tmp/tiered-pane").ok(),
                    alt_screen_active: false,
                    seqno: 11,
                    lines: vec![
                        Line::from_text("alpha", &Default::default(), 1, None),
                        Line::from_text("beta", &Default::default(), 1, None),
                    ],
                }),
            }
        }

        fn set_tiered_scrollback_status(&self, status: Option<PaneTieredScrollbackStatus>) {
            self.state.lock().unwrap().tiered_scrollback_status = status;
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            self.state.lock().unwrap().cursor_position
        }

        fn get_current_seqno(&self) -> SequenceNo {
            self.state.lock().unwrap().seqno
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let state = self.state.lock().unwrap();
            (
                lines.start,
                state
                    .lines
                    .iter()
                    .skip(lines.start as usize)
                    .take((lines.end - lines.start) as usize)
                    .cloned()
                    .collect(),
            )
        }

        fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
            mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            lines: Range<StableRowIndex>,
            for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
        }

        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            self.state.lock().unwrap().dimensions
        }

        fn get_tiered_scrollback_status(&self) -> Option<PaneTieredScrollbackStatus> {
            self.state.lock().unwrap().tiered_scrollback_status
        }

        fn get_title(&self) -> String {
            self.state.lock().unwrap().title.clone()
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!()
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.dimensions.cols = size.cols;
            state.dimensions.viewport_rows = size.rows;
            state.dimensions.scrollback_rows = size.rows;
            state.dimensions.dpi = size.dpi;
            state.dimensions.pixel_width = size.pixel_width;
            state.dimensions.pixel_height = size.pixel_height;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            DomainId::default()
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            self.state.lock().unwrap().alt_screen_active
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            self.state.lock().unwrap().working_dir.clone()
        }
    }

    fn sample_tiered_scrollback_status(cold_spill_lines_total: u64) -> PaneTieredScrollbackStatus {
        PaneTieredScrollbackStatus {
            tiering_enabled: true,
            configured_scrollback_rows: 10_000,
            configured_hot_lines: 512,
            configured_warm_max_bytes: 8 * 1024,
            visible_rows: 2,
            in_memory_scrollback_rows: 6,
            warm_resident_lines: 4,
            warm_resident_bytes: 512,
            warm_spill_lines_total: cold_spill_lines_total + 4,
            warm_spill_bytes_total: 4096,
            cold_spill_lines_total,
            cold_spill_bytes_total: cold_spill_lines_total * 64,
            cold_worker_peak_backlog_depth: 3,
            cold_worker_completion_throughput_lines_per_sec: 256,
            cold_worker_completed_lines_total: cold_spill_lines_total,
            cold_worker_completed_batches_total: 2,
            cold_worker_cancellation_count: 0,
        }
    }

    /// Creates a PduSender that captures all sent PDUs into a shared Vec.
    fn capturing_sender() -> (PduSender, Arc<Mutex<Vec<DecodedPdu>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let sender = PduSender::new(move |pdu| {
            captured_clone.lock().unwrap().push(pdu);
            Ok(())
        });
        (sender, captured)
    }

    /// Extract the single response PDU from the captured list.
    fn take_response(captured: &Arc<Mutex<Vec<DecodedPdu>>>) -> DecodedPdu {
        let mut v = captured.lock().unwrap();
        assert_eq!(v.len(), 1, "expected exactly one response PDU");
        v.remove(0)
    }

    fn tick_until_response(
        executor: &SimpleExecutor,
        captured: &Arc<Mutex<Vec<DecodedPdu>>>,
        expected: usize,
    ) {
        for _ in 0..16 {
            if captured.lock().unwrap().len() >= expected {
                return;
            }
            executor.tick().unwrap();
        }

        let observed = captured.lock().unwrap().len();
        panic!("timed out waiting for {expected} response PDUs; saw {observed}");
    }

    fn test_tab_size() -> TerminalSize {
        TerminalSize {
            rows: 40,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 1000,
            dpi: 96,
        }
    }

    fn install_tab_with_window(
        tab: &Arc<mux::tab::Tab>,
        extra_panes: &[Arc<dyn Pane>],
    ) -> (Arc<Mux>, ScopedMux) {
        let mux = Arc::new(Mux::new(None));
        let guard = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_and_active_pane(tab).unwrap();
        for pane in extra_panes {
            mux.add_pane(pane).unwrap();
        }
        mux.add_tab_to_window(tab, window_id).unwrap();
        drop(window);
        (mux, guard)
    }

    #[test]
    fn session_handler_new_has_empty_state() {
        let (sender, _captured) = capturing_sender();
        let handler = SessionHandler::new(sender);
        assert!(handler.client_id.is_none());
        assert!(handler.proxy_client_id.is_none());
        assert!(handler.per_pane.is_empty());
    }

    #[test]
    fn per_pane_creates_and_caches_entry() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let pp1 = handler.per_pane(42);
        let pp2 = handler.per_pane(42);
        // Same Arc returned for same pane_id
        assert!(Arc::ptr_eq(&pp1, &pp2));

        let pp3 = handler.per_pane(99);
        // Different pane_id gets a different entry
        assert!(!Arc::ptr_eq(&pp1, &pp3));
    }

    #[test]
    fn per_pane_default_has_zero_seqno_and_empty_title() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let pp = handler.per_pane(1);
        let guard = pp.lock().unwrap();
        assert_eq!(guard.seqno, 0);
        assert_eq!(guard.title, "");
        assert!(!guard.mouse_grabbed);
        assert!(!guard.sent_initial_palette);
        assert!(guard.notifications.is_empty());
        assert!(guard.working_dir.is_none());
    }

    #[test]
    fn ping_pdu_returns_pong() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 42,
            pdu: Pdu::Ping(Ping {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 42);
        assert_eq!(resp.pdu, Pdu::Pong(Pong {}));
    }

    #[test]
    fn select_stack_pane_pdu_selects_requested_stack_member() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane1: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        let pane2: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        let pane3: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(3, None));
        tab.assign_pane(&pane1);
        let first_request = mux::tab::SplitRequest {
            direction: mux::tab::SplitDirection::Horizontal,
            ..Default::default()
        };
        tab.split_and_insert(0, first_request, Arc::clone(&pane2))
            .unwrap();
        let second_request = mux::tab::SplitRequest {
            direction: mux::tab::SplitDirection::Horizontal,
            ..Default::default()
        };
        tab.split_and_insert(0, second_request, Arc::clone(&pane3))
            .unwrap();
        tab.set_layout_cycle(mux::layout::default_cycle());
        tab.swap_to_layout_index(2).unwrap();
        let (_mux, _guard) =
            install_tab_with_window(&tab, &[Arc::clone(&pane2), Arc::clone(&pane3)]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 100,
            pdu: Pdu::SelectStackPane(SelectStackPane {
                tab_id: tab.tab_id(),
                slot_index: 0,
                pane_index: 2,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 100);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        let visible = tab.iter_panes_ignoring_zoom();
        assert_eq!(visible.len(), 1);
        assert_ne!(visible[0].pane.pane_id(), 1);
    }

    #[test]
    fn update_pane_constraints_pdu_updates_effective_constraints() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7, None));
        tab.assign_pane(&pane);
        let (_mux, _guard) = install_tab_with_window(&tab, &[]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 101,
            pdu: Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 7,
                min_width: Some(20),
                max_width: Some(200),
                min_height: None,
                max_height: Some(50),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 101);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        let constraints = tab.effective_pane_constraints(7).unwrap();
        assert_eq!(constraints.min_width, 20);
        assert_eq!(constraints.max_width, Some(200));
        assert_eq!(constraints.min_height, 3);
        assert_eq!(constraints.max_height, Some(50));
    }

    #[test]
    fn floating_pane_pdus_update_live_tab_state() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let tiled: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        tab.assign_pane(&tiled);
        let (mux, _guard) = install_tab_with_window(&tab, &[]);
        let floating: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        mux.add_pane(&floating).unwrap();
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let rect = mux::tab::FloatingPaneRect {
            left: 4,
            top: 5,
            width: 30,
            height: 12,
        };
        handler.process_one(DecodedPdu {
            serial: 110,
            pdu: Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id: tab.tab_id(),
                pane_id: 2,
                rect,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(tab.has_floating_pane(2));
        let floating_panes = tab.iter_floating_panes();
        assert_eq!(floating_panes.len(), 1);
        assert_eq!(floating_panes[0].left, 4);

        let moved = mux::tab::FloatingPaneRect {
            left: 10,
            top: 7,
            width: 24,
            height: 10,
        };
        handler.process_one(DecodedPdu {
            serial: 111,
            pdu: Pdu::MoveFloatingPane(MoveFloatingPane {
                pane_id: 2,
                rect: moved,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        let floating_panes = tab.iter_floating_panes();
        assert_eq!(floating_panes[0].left, 10);
        assert_eq!(floating_panes[0].top, 7);

        handler.process_one(DecodedPdu {
            serial: 112,
            pdu: Pdu::SetFloatingPaneZ(SetFloatingPaneZ {
                pane_id: 2,
                z_order: 99,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert_eq!(tab.iter_floating_panes()[0].z_order, 99);

        handler.process_one(DecodedPdu {
            serial: 113,
            pdu: Pdu::ToggleFloatingPane(ToggleFloatingPane {
                pane_id: 2,
                visible: false,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(!tab.iter_floating_panes()[0].visible);

        handler.process_one(DecodedPdu {
            serial: 114,
            pdu: Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 2 }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(!tab.has_floating_pane(2));
    }

    #[test]
    fn invalid_pdu_returns_error_response() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 200,
            pdu: Pdu::Invalid { ident: 255 },
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 200);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("invalid PDU"),
                    "error should mention invalid PDU, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_pdu_treated_as_unexpected_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        // Sending a Pong (which is a response) as a request should get rejected
        handler.process_one(DecodedPdu {
            serial: 300,
            pdu: Pdu::Pong(Pong {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 300);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("expected a request"),
                    "error should mention expected request, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn unit_response_treated_as_unexpected_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 301,
            pdu: Pdu::UnitResponse(UnitResponse {}),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn pdu_sender_propagates_errors() {
        let sender = PduSender::new(|_| anyhow::bail!("send failed"));
        let result = sender.send(DecodedPdu {
            serial: 0,
            pdu: Pdu::Pong(Pong {}),
        });
        assert!(result.is_err());
    }

    #[test]
    fn serial_preserved_across_different_pdus() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        // Send multiple synchronous PDUs with distinct serials
        for serial in [1, 100, 999, u64::MAX] {
            handler.process_one(DecodedPdu {
                serial,
                pdu: Pdu::Ping(Ping {}),
            });
        }

        let pdus = captured.lock().unwrap();
        let serials: Vec<u64> = pdus.iter().map(|p| p.serial).collect();
        assert_eq!(serials, vec![1, 100, 999, u64::MAX]);
    }

    #[test]
    fn update_pane_constraints_all_none_keeps_default_constraints() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        tab.assign_pane(&pane);
        let (_mux, _guard) = install_tab_with_window(&tab, &[]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 50,
            pdu: Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 1,
                min_width: None,
                max_width: None,
                min_height: None,
                max_height: None,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert_eq!(
            tab.effective_pane_constraints(1).unwrap(),
            mux::pane::PaneConstraints::default()
        );
    }

    #[test]
    fn error_response_pdu_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 400,
            pdu: Pdu::ErrorResponse(ErrorResponse {
                reason: "test error".to_string(),
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn list_panes_response_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 401,
            pdu: Pdu::ListPanesResponse(ListPanesResponse {
                tabs: vec![],
                tab_titles: vec![],
                window_titles: HashMap::new(),
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    fn test_client_id(name: &str, pid: u32) -> ClientId {
        ClientId {
            hostname: format!("{name}.local"),
            username: "testuser".to_string(),
            pid,
            epoch: 1000,
            id: 0,
            ssh_auth_sock: None,
        }
    }

    #[test]
    fn set_client_id_proxy_stores_proxy_identity() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let proxy_id = test_client_id("proxy-host", 999);
        handler.process_one(DecodedPdu {
            serial: 500,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: proxy_id.clone(),
                is_proxy: true,
            }),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 500);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));

        // Proxy identity should be stored
        assert!(handler.proxy_client_id.is_some());
        let stored = handler.proxy_client_id.as_ref().unwrap();
        assert_eq!(stored.hostname, "proxy-host.local");
        assert_eq!(stored.pid, 999);
    }

    #[test]
    fn set_client_id_proxy_only_stores_first_proxy() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let first_proxy = test_client_id("first-proxy", 100);
        handler.process_one(DecodedPdu {
            serial: 1,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: first_proxy,
                is_proxy: true,
            }),
        });

        let second_proxy = test_client_id("second-proxy", 200);
        handler.process_one(DecodedPdu {
            serial: 2,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: second_proxy,
                is_proxy: true,
            }),
        });

        // First proxy should be kept, second ignored
        let stored = handler.proxy_client_id.as_ref().unwrap();
        assert_eq!(stored.hostname, "first-proxy.local");
        assert_eq!(stored.pid, 100);
    }

    #[test]
    fn set_client_id_replaces_prior_registered_client_without_leaking_stale_entries() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let first = test_client_id("review-first", 41_001);
        let second = test_client_id("review-second", 41_002);
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 10,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: first.clone(),
                is_proxy: false,
            }),
        });

        let clients_after_first_set = mux.iter_clients();
        assert!(
            clients_after_first_set
                .iter()
                .any(|info| *info.client_id == first)
        );

        handler.process_one(DecodedPdu {
            serial: 11,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: second.clone(),
                is_proxy: false,
            }),
        });

        let clients = mux.iter_clients();
        assert!(clients.iter().any(|info| *info.client_id == second));
        assert!(!clients.iter().any(|info| *info.client_id == first));

        drop(handler);

        let clients_after_drop = mux.iter_clients();
        assert!(
            !clients_after_drop
                .iter()
                .any(|info| *info.client_id == second)
        );
    }

    #[test]
    fn dropping_handler_after_mux_shutdown_does_not_panic() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client = test_client_id("shutdown-safe", 41_003);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 12,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client,
                is_proxy: false,
            }),
        });

        Mux::shutdown();
        drop(handler);
    }

    #[test]
    fn set_active_workspace_updates_registered_client_workspace() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client = test_client_id("workspace-owner", 41_004);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 21,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        let _ = take_response(&captured);

        handler.process_one(DecodedPdu {
            serial: 22,
            pdu: Pdu::SetActiveWorkspace(SetActiveWorkspace {
                workspace: "remote-dev".to_string(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 22);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));

        let client = Arc::new(client);
        assert_eq!(mux.active_workspace_for_client(&client), "remote-dev");
    }

    #[test]
    fn get_codec_version_returns_version_info() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 600,
            pdu: Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 600);
        match resp.pdu {
            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                codec_vers,
                version_string,
                executable_path,
                ..
            }) => {
                assert_eq!(codec_vers, CODEC_VERSION);
                assert!(
                    !version_string.is_empty(),
                    "version string should not be empty"
                );
                assert!(
                    !executable_path.to_string_lossy().is_empty(),
                    "executable path should be non-empty"
                );
            }
            other => panic!("expected GetCodecVersionResponse, got {other:?}"),
        }
    }

    #[test]
    fn catch_helper_forwards_ok() {
        let (sender, captured) = capturing_sender();
        let serial = 700;
        let send_response = {
            let sender = sender.clone();
            move |result: anyhow::Result<Pdu>| {
                let pdu = match result {
                    Ok(pdu) => pdu,
                    Err(err) => Pdu::ErrorResponse(ErrorResponse {
                        reason: format!("{err:#}"),
                    }),
                };
                sender.send(DecodedPdu { pdu, serial }).ok();
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        catch(|| Ok(Pdu::UnitResponse(UnitResponse {})), send_response);

        let resp = take_response(&captured);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
    }

    #[test]
    fn catch_helper_forwards_error() {
        let (sender, captured) = capturing_sender();
        let serial = 701;
        let send_response = {
            let sender = sender.clone();
            move |result: anyhow::Result<Pdu>| {
                let pdu = match result {
                    Ok(pdu) => pdu,
                    Err(err) => Pdu::ErrorResponse(ErrorResponse {
                        reason: format!("{err:#}"),
                    }),
                };
                sender.send(DecodedPdu { pdu, serial }).ok();
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        catch(|| anyhow::bail!("something went wrong"), send_response);

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("something went wrong"),
                    "error should propagate message, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn multiple_per_pane_entries_are_independent() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let pp1 = handler.per_pane(10);
        let pp2 = handler.per_pane(20);

        // Modify one, verify the other is unaffected
        pp1.lock().unwrap().seqno = 42;
        assert_eq!(pp2.lock().unwrap().seqno, 0);
    }

    #[test]
    fn compute_changes_includes_initial_tiered_scrollback_status_snapshot() {
        let pane = Arc::new(FakePane::new(Some(sample_tiered_scrollback_status(12))));
        let pane_dyn: Arc<dyn Pane> = pane;
        let mut per_pane = PerPane::default();

        let response = per_pane
            .compute_changes(&pane_dyn, None)
            .expect("initial pane snapshot should produce a response");

        assert_eq!(
            response.tiered_scrollback_status,
            Some(sample_tiered_scrollback_status(12))
        );
        assert_eq!(
            per_pane.tiered_scrollback_status,
            Some(sample_tiered_scrollback_status(12))
        );
    }

    #[test]
    fn compute_changes_detects_cleared_tiered_scrollback_status_without_other_deltas() {
        let pane = Arc::new(FakePane::new(Some(sample_tiered_scrollback_status(12))));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let mut per_pane = PerPane::default();

        let initial = per_pane.compute_changes(&pane_dyn, None);
        assert!(
            initial.is_some(),
            "first snapshot should populate cached pane state"
        );
        assert!(
            per_pane.compute_changes(&pane_dyn, None).is_none(),
            "unchanged pane state should not emit a redundant render delta"
        );

        pane.set_tiered_scrollback_status(None);

        let response = per_pane
            .compute_changes(&pane_dyn, None)
            .expect("clearing tiered scrollback status should produce a response");

        assert!(response.dirty_lines.is_empty());
        assert_eq!(response.tiered_scrollback_status, None);
        assert_eq!(per_pane.tiered_scrollback_status, None);
    }

    #[test]
    fn compute_changes_detects_alt_screen_transition_without_other_deltas() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let mut per_pane = PerPane::default();

        assert!(
            per_pane.compute_changes(&pane_dyn, None).is_some(),
            "first snapshot should populate cached pane state"
        );
        assert!(
            per_pane.compute_changes(&pane_dyn, None).is_none(),
            "unchanged pane state should not emit a redundant render delta"
        );

        pane.state.lock().unwrap().alt_screen_active = true;

        let response = per_pane
            .compute_changes(&pane_dyn, None)
            .expect("alt-screen transition should produce a response");

        assert!(response.alt_screen_active);
        assert!(per_pane.alt_screen_active);
    }

    #[test]
    fn compute_changes_skips_missing_cursor_row_in_bonus_lines() {
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().cursor_position.y = 99;
        let pane_dyn: Arc<dyn Pane> = pane;
        let mut per_pane = PerPane::default();

        let response = per_pane
            .compute_changes(&pane_dyn, None)
            .expect("initial pane snapshot should still produce a response");

        let cursor_y = response.cursor_position.y;
        let (bonus_lines, _images) = response.bonus_lines.extract_data();

        assert!(bonus_lines.is_empty());
        assert_eq!(cursor_y, 99);
    }
}
