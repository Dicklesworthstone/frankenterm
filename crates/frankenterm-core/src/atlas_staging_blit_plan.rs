//! Byte-accurate blit planning for atlas host-RAM staging.
//!
//! The wgpu integration consumes [`StagingTransferEvent`]s once per
//! frame. This module keeps the command planning independent of wgpu
//! types so tests can pin direction, offsets, and byte accounting
//! before the GUI layer emits copy commands.

use crate::atlas_tiered_swap::{StagingTransferDirection, StagingTransferEvent};

/// Direction a renderer copy command must perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AtlasBlitDirection {
    /// Copy atlas bytes from VRAM into host staging.
    VramToStaging,
    /// Copy staged host bytes back into VRAM.
    StagingToVram,
}

impl From<StagingTransferDirection> for AtlasBlitDirection {
    fn from(direction: StagingTransferDirection) -> Self {
        match direction {
            StagingTransferDirection::Demote => Self::VramToStaging,
            StagingTransferDirection::Promote => Self::StagingToVram,
        }
    }
}

/// A single blit command in renderer-neutral coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtlasBlitCommand {
    pub region_id: u64,
    pub direction: AtlasBlitDirection,
    pub staging_offset: u64,
    pub byte_len: u64,
    pub frame_id: u64,
}

impl AtlasBlitCommand {
    /// Build a command from the staged transfer event.
    #[must_use]
    pub fn from_event(event: StagingTransferEvent) -> Self {
        Self {
            region_id: event.region_id,
            direction: event.direction.into(),
            staging_offset: event.offset(),
            byte_len: event.bytes(),
            frame_id: event.frame_id,
        }
    }
}

/// Per-frame blit dispatch plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasBlitPlan {
    commands: Vec<AtlasBlitCommand>,
    download_bytes: u64,
    upload_bytes: u64,
}

impl AtlasBlitPlan {
    /// Build a plan from admitted transfer events.
    #[must_use]
    pub fn from_events(events: &[StagingTransferEvent]) -> Self {
        let mut commands = Vec::with_capacity(events.len());
        let mut download_bytes = 0_u64;
        let mut upload_bytes = 0_u64;

        for event in events {
            let command = AtlasBlitCommand::from_event(*event);
            match command.direction {
                AtlasBlitDirection::VramToStaging => {
                    download_bytes = download_bytes.saturating_add(command.byte_len);
                }
                AtlasBlitDirection::StagingToVram => {
                    upload_bytes = upload_bytes.saturating_add(command.byte_len);
                }
            }
            commands.push(command);
        }

        Self {
            commands,
            download_bytes,
            upload_bytes,
        }
    }

    /// Commands in the same order as the drained transfer events.
    #[must_use]
    pub fn commands(&self) -> &[AtlasBlitCommand] {
        &self.commands
    }

    /// Bytes copied from VRAM to host staging.
    #[must_use]
    pub const fn download_bytes(&self) -> u64 {
        self.download_bytes
    }

    /// Bytes copied from host staging to VRAM.
    #[must_use]
    pub const fn upload_bytes(&self) -> u64 {
        self.upload_bytes
    }

    /// Total bytes moved by this plan.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.download_bytes.saturating_add(self.upload_bytes)
    }

    /// Whether no blits are planned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas_tiered_swap::{
        HostRamStagingAllocation, StagingTransferDirection, StagingTransferEvent,
    };

    fn event(
        region_id: u64,
        direction: StagingTransferDirection,
        offset: u64,
        bytes: u64,
        frame_id: u64,
    ) -> StagingTransferEvent {
        StagingTransferEvent {
            region_id,
            direction,
            allocation: HostRamStagingAllocation {
                region_id,
                offset,
                bytes,
            },
            frame_id,
        }
    }

    #[test]
    fn empty_plan_has_no_commands_or_bytes() {
        let plan = AtlasBlitPlan::from_events(&[]);
        assert!(plan.is_empty());
        assert_eq!(plan.commands(), &[]);
        assert_eq!(plan.download_bytes(), 0);
        assert_eq!(plan.upload_bytes(), 0);
        assert_eq!(plan.total_bytes(), 0);
    }

    #[test]
    fn maps_demote_to_vram_download() {
        let plan =
            AtlasBlitPlan::from_events(&[event(7, StagingTransferDirection::Demote, 128, 512, 4)]);

        assert_eq!(
            plan.commands(),
            &[AtlasBlitCommand {
                region_id: 7,
                direction: AtlasBlitDirection::VramToStaging,
                staging_offset: 128,
                byte_len: 512,
                frame_id: 4,
            }]
        );
        assert_eq!(plan.download_bytes(), 512);
        assert_eq!(plan.upload_bytes(), 0);
    }

    #[test]
    fn maps_promote_to_vram_upload() {
        let plan =
            AtlasBlitPlan::from_events(&[event(9, StagingTransferDirection::Promote, 640, 256, 5)]);

        assert_eq!(
            plan.commands()[0],
            AtlasBlitCommand {
                region_id: 9,
                direction: AtlasBlitDirection::StagingToVram,
                staging_offset: 640,
                byte_len: 256,
                frame_id: 5,
            }
        );
        assert_eq!(plan.download_bytes(), 0);
        assert_eq!(plan.upload_bytes(), 256);
    }

    #[test]
    fn preserves_event_order_and_accounts_by_direction() {
        let events = [
            event(1, StagingTransferDirection::Demote, 0, 128, 10),
            event(2, StagingTransferDirection::Promote, 128, 64, 10),
            event(3, StagingTransferDirection::Demote, 192, 32, 11),
        ];

        let plan = AtlasBlitPlan::from_events(&events);

        let ids: Vec<u64> = plan.commands().iter().map(|cmd| cmd.region_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(plan.download_bytes(), 160);
        assert_eq!(plan.upload_bytes(), 64);
        assert_eq!(plan.total_bytes(), 224);
    }

    #[test]
    fn byte_counters_saturate_on_overflow() {
        let events = [
            event(1, StagingTransferDirection::Demote, 0, u64::MAX, 1),
            event(2, StagingTransferDirection::Promote, 0, 1, 1),
        ];

        let plan = AtlasBlitPlan::from_events(&events);

        assert_eq!(plan.download_bytes(), u64::MAX);
        assert_eq!(plan.upload_bytes(), 1);
        assert_eq!(plan.total_bytes(), u64::MAX);
    }
}
