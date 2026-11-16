//! [ft-aw52a / ft-dn2tu Phase 6] StorageHandle impl-split scaffolding.
//!
//! Phase 6 of the storage.rs library-vs-binary split. The
//! `pub struct StorageHandle` definition stays in `storage.rs`
//! alongside the writer-thread loop and the WriteCommand enum. Its
//! ~290 public async methods are being progressively split into
//! per-feature submodules so future readers don't have to hold the
//! entire 30k-line file in their head.
//!
//! ## Scaffolding model
//!
//! Each per-feature file holds a single `impl StorageHandle { ... }`
//! block carrying its method group. Because these submodules sit
//! under `storage::handle::*`, they are descendants of `storage` and
//! can therefore access StorageHandle's private fields
//! (`write_tx`, `db_path`, etc.) without widening their visibility.
//!
//! ## Beachhead
//!
//! - [`event_mutes`] — 8 async methods governing persistent event
//!   mute records (`add_event_mute`, `remove_event_mute`,
//!   `is_event_muted`, `list_active_mutes` + their `_with_cx`
//!   siblings). ~108 LOC. Chosen as the beachhead because the group
//!   is small, cohesive, and references exactly four public types
//!   (`EventMuteRecord`, `WriteCommand::{UpsertEventMute,
//!   DeleteEventMute}`, `PooledReadConn`, `Result`).
//!
//! ## Remaining work (filed as follow-ups)
//!
//! The other ~282 methods in the giant `impl StorageHandle` at the
//! top of `storage.rs` group naturally into roughly 12 feature
//! clusters (audit, segments, events, mutes, panes, workflows,
//! sessions, reservations, fts/search, timeline, retention/cleanup,
//! lifecycle/init). Each cluster is its own follow-up bead so the
//! splits ship as small reviewable chunks rather than a monolithic
//! 5K-line move that would drop a verification cycle on the floor.

mod event_mutes;
