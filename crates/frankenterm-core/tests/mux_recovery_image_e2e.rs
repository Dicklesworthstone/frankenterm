#![cfg(feature = "frankenterm-deps")]
#![recursion_limit = "256"]

//! Encrypted publication, authenticated graph closure, and inert reconstruction.
//! These tests use real parser-ground terminal checkpoints and captured-topology
//! fixtures. They do not claim live guardian takeover or remote durability.

use std::collections::HashSet;
use std::sync::Arc;

use frankenterm_core::cx::Cx;
use frankenterm_core::mux_recovery_image::{CheckpointAuthority, MuxRecoveryImage};
use frankenterm_core::session_restore::{
    ValidatedWholeMuxRecovery, WholeMuxRecoveryError, WholeMuxRecoveryVerifier,
    WholeMuxTrustedIdentityConfig, reconstruct_whole_mux_image_inert,
    validate_whole_mux_image_layout_only,
};
use frankenterm_core::snapshot_engine::{
    WholeMuxPanePublication, WholeMuxPublicationIdentity, publish_whole_mux_recovery,
};
use frankenterm_core::snapshot_publication::{
    GenerationPublicationReceipt, PredecessorBinding, RootSlotCandidate, SnapshotPublicationStore,
    sha256_hex,
};
use frankenterm_core::snapshot_repair::{
    ExpectedRecoveryIdentity, RepairAdmissionController, RepairError, RepairProtectionClass,
    decode_repair_symbols, encode_repair_envelope,
};
use frankenterm_core::snapshot_representation::{
    EncryptedRecoveryObject, ExpectedContext, ObjectMetadata, RecoveryKey, RecoveryObjectKind,
    decode_recovery_object, encode_recovery_object, representation_id_from_envelope_bytes,
};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::terminalstate::checkpoint::{TerminalCheckpointLimits, TerminalCheckpointV2};
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use mux::tab::{PaneEntry, PaneNode, SplitDirection, SplitDirectionAndSize};

const SESSION: &str = "recovery-integration-session";
const INCARNATION: [u8; 16] = [11; 16];
const ROOT_ID: [u8; 32] = [12; 32];

#[derive(Debug)]
struct NonDefaultConfig;

impl TerminalConfiguration for NonDefaultConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn scrollback_size(&self) -> usize {
        123
    }

    fn enable_kitty_keyboard(&self) -> bool {
        true
    }
}

fn size() -> TerminalSize {
    TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 640,
        pixel_height: 384,
        dpi: 96,
    }
}

struct Fixture {
    captured: mux::MuxCapturedTopology,
    acks: Vec<mux::ModelParserCheckpointAck>,
    key: Arc<RecoveryKey>,
}

impl Fixture {
    fn new() -> Self {
        let acks: Vec<_> = (0..8)
            .map(|pane_id| {
                let mut terminal = Terminal::new(
                    size(),
                    Arc::new(NonDefaultConfig),
                    "FrankenTerm",
                    "recovery-integration",
                    Box::new(Vec::<u8>::new()),
                );
                for line in 0..40 {
                    terminal.advance_bytes(format!("pane {pane_id} line {line}\r\n").as_bytes());
                }
                if pane_id == 2 {
                    terminal.advance_bytes(b"\x1b[?1049hALT monitor\r\n");
                }
                terminal.advance_bytes(b"\x1b[?2004h\x1b[31mcolored Unicode: \xe7\x95\x8c\r\n");
                let terminal_checkpoint = terminal
                    .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
                    .expect("real parser-ground capture");
                mux::ModelParserCheckpointAck {
                    registration_wire_identity: (pane_id as u128 + 1).to_be_bytes(),
                    durable_pane_id: uuid::Uuid::from_u128(pane_id as u128 + 1),
                    parser_stream_bytes: terminal_checkpoint.parser_stream_bytes(),
                    terminal_checkpoint,
                }
            })
            .collect();
        let tab_ids = [10, 10, 11, 12, 12, 13, 14, 11];
        let cursors: Vec<_> = acks
            .iter()
            .map(|ack| {
                let validated = TerminalCheckpointV2::decode_canonical_json(
                    ack.terminal_checkpoint.canonical_payload(),
                    TerminalCheckpointLimits::default(),
                )
                .unwrap();
                let (row, column) = validated.checkpoint().cursor_position();
                (
                    usize::try_from(row).unwrap(),
                    usize::try_from(column).unwrap(),
                )
            })
            .collect();
        let pane_bindings = acks
            .iter()
            .enumerate()
            .map(|(id, ack)| mux::MuxCapturedPaneBinding {
                pane_id: id,
                pane_uuid: ack.durable_pane_id.to_string(),
                registration_wire_identity: ack.registration_wire_identity,
                domain_id: if (3..=5).contains(&id) { 2 } else { 1 },
                domain_name: if (3..=5).contains(&id) {
                    "remote"
                } else {
                    "local"
                }
                .into(),
                window_id: if matches!(id, 0 | 1 | 2 | 7) { 1 } else { 2 },
                tab_id: tab_ids[id],
                lane: if id == 7 {
                    mux::MuxCapturedPaneLane::Floating
                } else {
                    mux::MuxCapturedPaneLane::Tiled
                },
                title: format!("pane-{id}"),
                cwd: None,
                size: size(),
                alt_screen_active: id == 2,
                cursor_pos: cursors[id],
                is_active_in_tab: !matches!(id, 1 | 4 | 7),
                is_zoomed_in_tab: false,
            })
            .collect();
        let leaf = |id: usize| {
            PaneNode::Leaf(PaneEntry {
                window_id: if matches!(id, 0 | 1 | 2 | 7) { 1 } else { 2 },
                tab_id: tab_ids[id],
                pane_id: id,
                title: format!("pane-{id}"),
                size: size(),
                working_dir: None,
                alt_screen_active: id == 2,
                is_active_pane: !matches!(id, 1 | 4 | 7),
                is_zoomed_pane: false,
                workspace: "default".into(),
                cursor_pos: Default::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: None,
            })
        };
        let tabs = [
            (10, 0, Some(1)),
            (11, 2, None),
            (12, 3, Some(4)),
            (13, 5, None),
            (14, 6, None),
        ]
        .into_iter()
        .map(|(tab_id, first, second)| {
            let mut tab_size = size();
            let split_tree = if let Some(second) = second {
                tab_size.cols = 161;
                tab_size.pixel_width = 1288;
                PaneNode::Split {
                    left: Box::new(leaf(first)),
                    right: Box::new(leaf(second)),
                    node: SplitDirectionAndSize {
                        direction: SplitDirection::Horizontal,
                        first: size(),
                        second: size(),
                    },
                }
            } else {
                leaf(first)
            };
            mux::MuxCapturedTab {
                tab_id,
                window_id: if tab_id <= 11 { 1 } else { 2 },
                title: format!("tab-{tab_id}"),
                size: tab_size,
                size_before_zoom: tab_size,
                active_pane_id: Some(first),
                zoomed_pane_id: None,
                split_tree,
                floating_panes: if tab_id == 11 {
                    vec![mux::MuxCapturedFloatingPane {
                        pane_id: 7,
                        rect: mux::tab::FloatingPaneRect {
                            left: 0,
                            top: 0,
                            width: 80,
                            height: 24,
                        },
                        z_order: 1,
                        visible: true,
                        pinned: false,
                        opacity: 0.75,
                        is_focused: true,
                    }]
                } else {
                    vec![]
                },
                floating_focus: (tab_id == 11).then_some(7),
                pane_stacks: vec![],
            }
        })
        .collect();
        let windows = [(1, vec![10, 11], 4), (2, vec![12, 13, 14], 4)]
            .into_iter()
            .map(
                |(window_id, ordered_tab_ids, structural_pane_count)| mux::MuxCapturedWindow {
                    window_id,
                    workspace: "default".into(),
                    title: format!("window-{window_id}"),
                    order_revision: mux::window::WindowOrderRevision::new(2),
                    active_tab_id: Some(ordered_tab_ids[0]),
                    active_tab_index: Some(0),
                    ordered_tab_ids,
                    position: None,
                    structural_pane_count,
                },
            )
            .collect();
        Self {
            captured: mux::MuxCapturedTopology {
                session_incarnation: mux::MuxSessionIncarnation::from_bytes(INCARNATION),
                topology_revision: mux::TopologyRevision::new(7),
                captured_at_epoch_ms: 1_700_000_000_000,
                client_workspace: Some(mux::MuxCapturedClientWorkspaceBinding {
                    client_id: "client".into(),
                    active_workspace: "default".into(),
                }),
                default_workspace: "default".into(),
                workspaces: vec![mux::MuxCapturedWorkspace {
                    name: "default".into(),
                    window_ids: vec![1, 2],
                    active_window_id: Some(1),
                    pane_count: 8,
                }],
                windows,
                tabs,
                pane_bindings,
            },
            acks,
            key: Arc::new(RecoveryKey::from_bytes([13; 32]).unwrap()),
        }
    }

    fn verifier(&self) -> WholeMuxRecoveryVerifier {
        WholeMuxRecoveryVerifier::new_production(
            Arc::clone(&self.key),
            WholeMuxTrustedIdentityConfig::new(ROOT_ID)
                .with_session_id(SESSION)
                .with_mux_incarnation_id(hex::encode(INCARNATION)),
        )
    }

    fn publish(
        &self,
        cx: &Cx,
        store: &SnapshotPublicationStore,
        predecessor: Option<(&GenerationPublicationReceipt, &ValidatedWholeMuxRecovery)>,
    ) -> GenerationPublicationReceipt {
        let generation = predecessor.map_or(1, |(receipt, _)| receipt.generation + 1);
        let ids: Vec<_> = (0..8)
            .map(|id| format!("generation-{generation}-pane-{id}"))
            .collect();
        let inputs: Vec<_> = self
            .acks
            .iter()
            .enumerate()
            .map(|(pane_id, ack)| WholeMuxPanePublication {
                pane_id,
                object_id: &ids[pane_id],
                ack,
            })
            .collect();
        publish_whole_mux_recovery(
            cx,
            store,
            &self.captured,
            &inputs,
            Arc::clone(&self.key),
            &WholeMuxPublicationIdentity {
                generation,
                session_id: SESSION.into(),
                mux_incarnation_id: hex::encode(INCARNATION),
                root_object_id: ROOT_ID,
                publisher_id: "integration-publisher".into(),
                ft_version: "test".into(),
                predecessor: predecessor.map(|(receipt, _)| PredecessorBinding {
                    expected_generation: receipt.generation,
                    expected_hash: receipt.sha256.clone(),
                }),
                predecessor_image_digest: predecessor
                    .map(|(_, verified)| verified.image().image_digest),
            },
        )
        .expect("actual encrypted publisher")
    }

    fn current(&self, cx: &Cx, store: &SnapshotPublicationStore) -> ValidatedWholeMuxRecovery {
        let verifier = self.verifier();
        store
            .select_verified_roots(
                &|candidate: &RootSlotCandidate, store: &SnapshotPublicationStore| {
                    verifier.verify_root_with_cx(cx, candidate, store)
                },
            )
            .unwrap()
            .current
            .expect("verified current generation")
    }
}

fn store() -> (tempfile::TempDir, SnapshotPublicationStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SnapshotPublicationStore::open(directory.path(), Default::default()).unwrap();
    (directory, store)
}

fn encrypted_candidate(image: &MuxRecoveryImage, key: &RecoveryKey) -> RootSlotCandidate {
    let plaintext = zeroize::Zeroizing::new(image.to_canonical_json().unwrap());
    let manifest_bytes = encode_recovery_object(
        &plaintext,
        ObjectMetadata::single(
            ROOT_ID,
            RecoveryObjectKind::WholeMuxImage,
            image.header.generation,
            None,
            image.header.created_at_epoch_ms,
        ),
        key,
        None,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    RootSlotCandidate {
        slot: frankenterm_core::snapshot_publication::RootSlot::SlotA,
        generation: image.header.generation,
        publisher_id: "integration-publisher".into(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: sha256_hex(&manifest_bytes),
        file_len: manifest_bytes.len() as u64,
        manifest_bytes,
        created_at_ms: image.header.created_at_epoch_ms,
    }
}

#[test]
fn test_mux_recovery_image_e2e_full_positive_journey() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let verified = fixture.current(&cx, &store);
    assert_eq!(verified.pane_count(), 8);
    assert_eq!(verified.checkpoint_payload_count(), 8);
    assert_eq!(verified.image().topology.windows.len(), 2);
    assert_eq!(verified.image().topology.domains.len(), 2);
    assert_eq!(
        verified.image().topology.windows[0].tabs[1].floating_panes[0].pane_id,
        7
    );
    let reconstructed = reconstruct_whole_mux_image_inert(
        &verified,
        TerminalCheckpointLimits::default(),
        Some("offline"),
        &HashSet::new(),
    )
    .expect("inert reconstruction with checkpoint's non-default configuration");
    let tabs = [10, 10, 11, 12, 12, 13, 14, 11];
    for (id, ack) in fixture.acks.iter().enumerate() {
        let pane = &reconstructed.pane_terminals[&(id as u64)];
        assert_eq!(pane.tab_id, tabs[id]);
        assert_eq!(pane.domain_id, if (3..=5).contains(&id) { 2 } else { 1 });
        assert_eq!((pane.rows, pane.cols), (24, 80));
        let restored = pane.terminal.checkpoint().unwrap();
        assert_eq!(restored.is_alternate_screen_active(), id == 2);
        assert!(restored.bracketed_paste());
        let canonical = restored
            .to_canonical_json(TerminalCheckpointLimits::default())
            .unwrap();
        assert_eq!(
            canonical.as_slice(),
            ack.terminal_checkpoint.canonical_payload(),
            "pane {id}: semantic state, scrollback, and non-default replay configuration must roundtrip"
        );
        let reference = &verified.image().panes[id].checkpoint.checkpoint_ref;
        let ciphertext = store.read_object(&reference.object_id).unwrap();
        assert_ne!(
            ciphertext.as_slice(),
            ack.terminal_checkpoint.canonical_payload()
        );
        assert_eq!(
            representation_id_from_envelope_bytes(&ciphertext),
            reference.payload_digest
        );
    }
}

#[test]
fn test_mux_recovery_e2e_torn_generation_falls_back_to_intact_predecessor() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    let first = fixture.publish(&cx, &store, None);
    let first_verified = fixture.current(&cx, &store);
    let second = fixture.publish(&cx, &store, Some((&first, &first_verified)));
    assert_eq!(fixture.current(&cx, &store).generation(), 2);
    // Corrupt only the test-owned second root; retain both files and all objects.
    let original = std::fs::read(&second.path).unwrap();
    std::fs::write(&second.path, &original[..original.len() / 2]).unwrap();
    assert_eq!(fixture.current(&cx, &store).generation(), 1);
    assert!(first.path.exists());
    assert!(second.path.exists());
    // Restore the exact bytes to prove corruption, rather than an invalid gen2,
    // caused selection to fall back.
    std::fs::write(&second.path, &original).unwrap();
    assert_eq!(fixture.current(&cx, &store).generation(), 2);
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_tampered_object() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    let first = fixture.publish(&cx, &store, None);
    let first_verified = fixture.current(&cx, &store);
    fixture.publish(&cx, &store, Some((&first, &first_verified)));
    let verified = fixture.current(&cx, &store);
    let object = &verified.image().panes[0].checkpoint.checkpoint_ref;
    let path = store
        .root_path()
        .join("objects")
        .join(format!("{}.obj", object.object_id));
    let mut bytes = std::fs::read(&path).unwrap();
    let original = bytes.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    let candidate = store
        .inspect_root_candidates()
        .unwrap()
        .0
        .into_iter()
        .find(|c| c.generation == 2)
        .unwrap();
    assert!(matches!(
        fixture
            .verifier()
            .verify_root_with_cx(&cx, &candidate, &store),
        Err(WholeMuxRecoveryError::CheckpointDigestMismatch { .. })
    ));
    assert_eq!(fixture.current(&cx, &store).generation(), 1);
    std::fs::write(&path, original).unwrap();
    assert_eq!(fixture.current(&cx, &store).generation(), 2);
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_missing_object() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let mut image = fixture.current(&cx, &store).image().clone();
    image.panes[3].checkpoint.checkpoint_ref.object_id = "absent-object".into();
    image.image_digest = image.compute_digest().unwrap();
    let candidate = encrypted_candidate(&image, &fixture.key);
    assert!(
        matches!(fixture.verifier().verify_root_with_cx(&cx, &candidate, &store),
        Err(WholeMuxRecoveryError::MissingCheckpointObject(id)) if id == "absent-object")
    );
    assert_eq!(fixture.current(&cx, &store).generation(), 1);
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_fake_guardian() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let mut image = fixture.current(&cx, &store).image().clone();
    image.panes[1].checkpoint.authority = CheckpointAuthority::Guardian {
        guardian_generation: 1,
        lease_verifier: "not-a-proof".into(),
        catalog_generation: 1,
    };
    image.image_digest = image.compute_digest().unwrap();
    let candidate = encrypted_candidate(&image, &fixture.key);
    assert!(matches!(
        fixture
            .verifier()
            .verify_root_with_cx(&cx, &candidate, &store),
        Err(WholeMuxRecoveryError::UnprovedGuardianAuthority { pane_id: 1, .. })
    ));
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_corrupted_manifest() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let mut candidate = store.inspect_root_candidates().unwrap().0.pop().unwrap();
    candidate.manifest_bytes = b"plaintext is never an authenticated recovery image".to_vec();
    candidate.manifest_sha256 = sha256_hex(&candidate.manifest_bytes);
    assert!(
        fixture
            .verifier()
            .verify_root_with_cx(&cx, &candidate, &store)
            .is_err()
    );
    assert_eq!(fixture.current(&cx, &store).generation(), 1);
}

#[test]
fn test_mux_recovery_e2e_wrong_key_fails_closed() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let candidate = store.inspect_root_candidates().unwrap().0.pop().unwrap();
    let wrong = WholeMuxRecoveryVerifier::new_production(
        Arc::new(RecoveryKey::from_bytes([99; 32]).unwrap()),
        WholeMuxTrustedIdentityConfig::new(ROOT_ID).with_session_id(SESSION),
    );
    assert!(wrong.verify_root_with_cx(&cx, &candidate, &store).is_err());
    assert!(
        fixture
            .verifier()
            .verify_root_with_cx(&cx, &candidate, &store)
            .is_ok()
    );
}

#[test]
fn test_mux_recovery_e2e_ciphertext_tamper_fails_closed() {
    let key = RecoveryKey::from_bytes([14; 32]).unwrap();
    let expected =
        ExpectedContext::single([15; 32], RecoveryObjectKind::TerminalCheckpoint, 1, None);
    let object = encode_recovery_object(
        b"secret",
        ObjectMetadata::single(
            [15; 32],
            RecoveryObjectKind::TerminalCheckpoint,
            1,
            None,
            100,
        ),
        &key,
        None,
    )
    .unwrap();
    assert_eq!(
        decode_recovery_object(&object, &expected, &key, None)
            .unwrap()
            .plaintext(),
        b"secret"
    );
    let mut bytes = object.to_bytes().unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    if let Ok(object) = EncryptedRecoveryObject::from_bytes(&bytes) {
        assert!(decode_recovery_object(&object, &expected, &key, None).is_err());
    }
}

#[test]
fn test_mux_recovery_e2e_fec_insufficient_rank_fails_closed() {
    let cx = frankenterm_core::cx::for_request();
    let key = RecoveryKey::from_bytes([16; 32]).unwrap();
    let plaintext: Vec<u8> = (0..4096).map(|i| ((i * 37 + i / 7) % 251) as u8).collect();
    let object = encode_recovery_object(
        &plaintext,
        ObjectMetadata::single(
            [17; 32],
            RecoveryObjectKind::TerminalCheckpoint,
            1,
            None,
            100,
        ),
        &key,
        None,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    let admission = RepairAdmissionController::default_production();
    let auth_key = [18; 32];
    let bundle = encode_repair_envelope(
        &cx,
        &object,
        [17; 32],
        1,
        &auth_key,
        64,
        RepairProtectionClass::Maximum,
        &admission,
    )
    .unwrap();
    assert!(bundle.manifest.k > 1);
    let expected =
        ExpectedRecoveryIdentity::new(representation_id_from_envelope_bytes(&object), [17; 32], 1);
    assert!(matches!(
        decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols[..1],
            &expected,
            &auth_key,
            &admission
        ),
        Err(RepairError::InsufficientRank { .. })
    ));
    let restored = decode_repair_symbols(
        &cx,
        &bundle.manifest,
        &bundle.symbols[1..],
        &expected,
        &auth_key,
        &admission,
    )
    .expect("authenticated erasure recovery");
    assert_eq!(restored.reconstructed_envelope, object);
}

#[test]
fn test_mux_recovery_e2e_reconstruct_refuses_active_live_destination() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let verified = fixture.current(&cx, &store);
    assert!(matches!(
        reconstruct_whole_mux_image_inert(
            &verified,
            TerminalCheckpointLimits::default(),
            Some("live"),
            &HashSet::from(["live".into()])
        ),
        Err(WholeMuxRecoveryError::LiveDestinationRefused { .. })
    ));
}

#[test]
fn test_mux_recovery_e2e_layout_only_validation() {
    let fixture = Fixture::new();
    let cx = frankenterm_core::cx::for_request();
    let (_directory, store) = store();
    fixture.publish(&cx, &store, None);
    let verified = fixture.current(&cx, &store);
    let active = HashSet::from(["live".into()]);
    assert!(matches!(
        validate_whole_mux_image_layout_only(&verified, Some("live"), &active),
        Err(WholeMuxRecoveryError::LiveDestinationRefused { .. })
    ));
    assert!(validate_whole_mux_image_layout_only(&verified, Some("offline"), &active).is_ok());
}
