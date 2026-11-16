//! One connection owns every read in a native browser snapshot transaction.

use super::{
    DirectMuxClient, DirectMuxError, cancelled_mux_error, checkpoint_mux_cx, classify_cx_timeout,
};
use crate::cx::Cx;
use crate::web::terminal_projection::{
    TerminalProjectionError, TerminalSnapshotBuilder, projected_dimensions, scope_for_identity,
};
use codec::{
    GetLinesAtLayout, GetPaneRenderChangesResponse, GetPaneRenderStateV1, LineReadLayout,
    PaneIdentityOutcomeV1, PaneIdentityV1, PaneRenderMetadataV1, Pdu,
};
use frankenterm_terminal_wire::{FrozenSnapshot, Scope, WireU64};
use std::time::Duration;

const MAX_SNAPSHOT_ATTEMPTS: usize = 3;
const MAX_SNAPSHOT_DURATION: Duration = Duration::from_secs(15);

/// Dropping a borrowed operation must not leave its caller an apparently
/// reusable client whose last request or reply was only partly transferred.
struct SnapshotTransaction<'a> {
    client: &'a mut DirectMuxClient,
    completed: bool,
}

impl Drop for SnapshotTransaction<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.client
                .poison_connection("terminal snapshot abandoned", true);
        }
    }
}

/// Retain only the fields needed for coherence. Bonus rows from the render
/// query are released immediately instead of retaining another native image.
#[derive(PartialEq)]
struct RenderFence {
    layout: LineReadLayout,
    cursor: mux::renderable::StableCursorPosition,
    mouse_grabbed: bool,
    alt_screen_active: bool,
    title: String,
    cwd: Option<String>,
}

impl RenderFence {
    fn from_native(
        response: GetPaneRenderChangesResponse,
    ) -> Result<Self, TerminalProjectionError> {
        if response.title.len() > 4096
            || response
                .working_dir
                .as_ref()
                .is_some_and(|cwd| cwd.as_str().len() > 4096)
        {
            return Err(TerminalProjectionError::LimitExceeded);
        }
        Ok(Self {
            layout: LineReadLayout {
                seqno: response.seqno,
                dimensions: response.dimensions,
            },
            cursor: response.cursor_position,
            mouse_grabbed: response.mouse_grabbed,
            alt_screen_active: response.alt_screen_active,
            title: response.title,
            cwd: response.working_dir.map(|cwd| cwd.as_str().to_string()),
        })
    }

    fn matches_metadata(&self, metadata: &PaneRenderMetadataV1) -> bool {
        self.layout.seqno == metadata.seqno
            && self.layout.dimensions == metadata.dimensions
            && self.cursor == metadata.cursor
            && self.mouse_grabbed == metadata.mouse_grabbed
            && self.alt_screen_active == metadata.alt_screen_active
    }
}

fn retryable_snapshot_failure(error: &TerminalProjectionError) -> bool {
    match error {
        TerminalProjectionError::SnapshotChanged => true,
        TerminalProjectionError::Native(DirectMuxError::RemoteRejection(reply)) => {
            reply.validate().is_ok()
                && reply.effect == codec::MuxErrorEffect::NOT_APPLIED
                && reply.retry == codec::MuxErrorRetry::SAFE_AFTER_BACKOFF
                && matches!(
                    reply.code,
                    codec::MuxErrorCode::RESOURCE_BUSY | codec::MuxErrorCode::BACKEND_FAILURE
                )
        }
        _ => false,
    }
}

impl DirectMuxClient {
    /// Capture the already-existing pane with exact identity and screen
    /// fences. This does not authorize access or bind a grant. No partial
    /// image survives a failed attempt, and no legacy read can substitute.
    pub async fn get_terminal_snapshot_with_cx(
        &mut self,
        cx: &Cx,
        pane_id: u64,
        expected_identity: &PaneIdentityV1,
        scope: Scope,
        snapshot_id: WireU64,
    ) -> Result<FrozenSnapshot, TerminalProjectionError> {
        checkpoint_mux_cx(cx, self.connection_id, "terminal_snapshot_admission")?;
        if !self
            .peer_generation
            .as_ref()
            .is_some_and(|peer| peer.agreed >= codec::GET_PANE_IDENTITY_V1_MIN_CODEC_VERSION)
        {
            return Err(TerminalProjectionError::UnsupportedPeer);
        }
        let native_pane_id =
            usize::try_from(pane_id).map_err(|_| TerminalProjectionError::InvalidSource)?;
        if scope_for_identity(scope.identity.host.clone(), expected_identity, scope.epoch)? != scope
        {
            return Err(TerminalProjectionError::IdentityChanged);
        }
        let budget = self
            .config
            .write_timeout
            .saturating_add(self.config.read_timeout)
            .min(MAX_SNAPSHOT_DURATION);
        let mut transaction = SnapshotTransaction {
            client: self,
            completed: false,
        };
        let result = crate::runtime_async::timeout_with_cx(cx, budget, async {
            for attempt in 0..MAX_SNAPSHOT_ATTEMPTS {
                checkpoint_mux_cx(
                    cx,
                    transaction.client.connection_id,
                    "terminal_snapshot_attempt",
                )?;
                let result = transaction
                    .client
                    .capture_terminal_attempt(
                        cx,
                        native_pane_id,
                        expected_identity,
                        scope.clone(),
                        snapshot_id,
                    )
                    .await;
                match result {
                    Err(error)
                        if attempt + 1 < MAX_SNAPSHOT_ATTEMPTS
                            && retryable_snapshot_failure(&error) =>
                    {
                        crate::runtime_async::sleep_with_cx(cx, Duration::from_millis(10))
                            .await
                            .map_err(|error| {
                                cancelled_mux_error("terminal_snapshot_backoff", error)
                            })?;
                    }
                    result => return result,
                }
            }
            Err(TerminalProjectionError::SnapshotChanged)
        })
        .await;
        let result = match result {
            Ok(result) => {
                checkpoint_mux_cx(
                    cx,
                    transaction.client.connection_id,
                    "terminal_snapshot_complete",
                )?;
                result
            }
            Err(timeout) => {
                let error = classify_cx_timeout(
                    cx,
                    "terminal_snapshot",
                    timeout,
                    DirectMuxError::ReadTimeout,
                );
                transaction
                    .client
                    .poison_connection("terminal snapshot budget expired", true);
                Err(error.into())
            }
        };
        transaction.completed = true;
        result
    }

    async fn terminal_identity_metadata(
        &mut self,
        cx: &Cx,
        pane_id: usize,
        expected_identity: &PaneIdentityV1,
    ) -> Result<PaneRenderMetadataV1, TerminalProjectionError> {
        let response = self.get_pane_identity_with_cx(cx, pane_id as u64).await?;
        match response.outcome {
            PaneIdentityOutcomeV1::Available {
                identity,
                render_facts,
            } => {
                if identity != *expected_identity {
                    return Err(TerminalProjectionError::IdentityChanged);
                }
                render_facts
                    .map(|facts| *facts)
                    .ok_or(TerminalProjectionError::UnsupportedPane)
            }
            PaneIdentityOutcomeV1::PaneGone => Err(TerminalProjectionError::IdentityChanged),
            PaneIdentityOutcomeV1::Unavailable => Err(TerminalProjectionError::IdentityUnavailable),
        }
    }

    async fn terminal_render_fence(
        &mut self,
        cx: &Cx,
        pane_id: usize,
    ) -> Result<RenderFence, TerminalProjectionError> {
        let response = self
            .send_request_with_cx(
                cx,
                Pdu::GetPaneRenderStateV1(GetPaneRenderStateV1 { pane_id }),
            )
            .await?;
        match response {
            Pdu::GetPaneRenderChangesResponse(response) if response.pane_id == pane_id => {
                RenderFence::from_native(response)
            }
            _ => Err(TerminalProjectionError::InvalidSource),
        }
    }

    async fn capture_terminal_attempt(
        &mut self,
        cx: &Cx,
        pane_id: usize,
        expected_identity: &PaneIdentityV1,
        scope: Scope,
        snapshot_id: WireU64,
    ) -> Result<FrozenSnapshot, TerminalProjectionError> {
        let initial_metadata = self
            .terminal_identity_metadata(cx, pane_id, expected_identity)
            .await?;
        let dimensions = projected_dimensions(&initial_metadata)?;
        let initial = self.terminal_render_fence(cx, pane_id).await?;
        if !initial.matches_metadata(&initial_metadata) {
            return Err(TerminalProjectionError::SnapshotChanged);
        }
        let mut builder = TerminalSnapshotBuilder::new(
            scope,
            snapshot_id,
            &initial_metadata,
            initial.title.clone(),
            initial.cwd.clone(),
        )?;
        let first = initial.layout.dimensions.physical_top;
        let visible_rows =
            isize::try_from(dimensions.rows).map_err(|_| TerminalProjectionError::InvalidSource)?;
        let end = first
            .checked_add(visible_rows)
            .ok_or(TerminalProjectionError::InvalidSource)?;
        let rows_per_chunk = frankenterm_terminal_wire::MAX_CHUNK_LINES
            .min(frankenterm_terminal_wire::MAX_CHUNK_CELLS / usize::from(dimensions.columns));
        let rows_per_chunk =
            isize::try_from(rows_per_chunk).map_err(|_| TerminalProjectionError::LimitExceeded)?;
        if rows_per_chunk == 0 {
            return Err(TerminalProjectionError::LimitExceeded);
        }
        let mut start = first;
        while start < end {
            checkpoint_mux_cx(cx, self.connection_id, "terminal_snapshot_rows")?;
            let chunk_end = start.checked_add(rows_per_chunk).unwrap_or(end).min(end);
            let response = self
                .send_request_with_cx(
                    cx,
                    Pdu::GetLinesAtLayout(GetLinesAtLayout {
                        pane_id,
                        layout: initial.layout,
                        lines: std::iter::once(start..chunk_end).collect(),
                    }),
                )
                .await?;
            let Pdu::GetLinesAtLayoutResponse(response) = response else {
                return Err(TerminalProjectionError::InvalidSource);
            };
            if response.pane_id != pane_id {
                return Err(TerminalProjectionError::InvalidSource);
            }
            if response.layout != initial.layout {
                return Err(TerminalProjectionError::SnapshotChanged);
            }
            let (rows, images) = response.lines.extract_data();
            if !images.is_empty() {
                return Err(TerminalProjectionError::Unsupported("image_placement"));
            }
            if rows.len() != usize::try_from(chunk_end - start).unwrap_or(usize::MAX)
                || rows
                    .iter()
                    .enumerate()
                    .any(|(index, (row, _))| start.checked_add(index as isize) != Some(*row))
            {
                return Err(TerminalProjectionError::InvalidSource);
            }
            builder.push_rows(rows)?;
            start = chunk_end;
        }
        let final_render = self.terminal_render_fence(cx, pane_id).await?;
        let final_metadata = self
            .terminal_identity_metadata(cx, pane_id, expected_identity)
            .await?;
        if final_render != initial
            || final_metadata != initial_metadata
            || !final_render.matches_metadata(&final_metadata)
        {
            return Err(TerminalProjectionError::SnapshotChanged);
        }
        checkpoint_mux_cx(cx, self.connection_id, "terminal_snapshot_publish")?;
        builder.finish()
    }
}
