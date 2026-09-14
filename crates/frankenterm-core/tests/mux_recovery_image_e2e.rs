#![cfg(feature = "frankenterm-deps")]
#![recursion_limit = "256"]

//! Independent End-to-End Integration and Semantic Crash/Corruption Recovery Tests.
//!
//! Bead: `ft-interactive-swarm-product-convergence-7xqz4.8.14.4.9.4`
//!
//! Exercises the full multi-tier recovery pipeline across production modules:
//! - `%574`: Canonical bounded whole-mux schema modeling, splits, floating panes, and validation (`mux_recovery_image`).
//! - `%573`: Real terminal state checkpointing, canonical JSON roundtrip, and inert reconstruction (`frankenterm_term`).
//! - `%571`: Bounded zstd compression, XChaCha20Poly1305 AEAD, and versioned AAD binding (`snapshot_representation`).
//! - `%570`: Authenticated RaptorQ FEC encoding, loss recovery, and inactivation rank solving (`snapshot_repair`).
//! - `%572`: Atomic dual-slot non-clobbering publication, predecessor lineage, and complete object graph verification (`snapshot_publication`).
//! - `%576`: Offline inert session reconstruction (`reconstruct_whole_mux_image_inert`), non-zero tab/domain bindings,
//!   and production root verification (`WholeMuxRecoveryVerifier`).
//! - Complete negative matrix: insufficient FEC rank, ciphertext tampering, wrong key rejection, torn generation fallback,
//!   missing referenced objects, payload digest mismatches, fake guardian authority, and live target safety gates.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use frankenterm_core::mux_recovery_image::{
    CheckpointAuthority, ClientWorkspaceBinding, FloatingPaneRect, MuxRecoveryImage,
    MuxRecoveryImageError, PaneCheckpointBinding, ParserCaptureIdentity, RecoveryDomain,
    RecoveryFloatingPane, RecoveryGuiPosition, RecoveryImageHeader, RecoveryObjectRef,
    RecoveryPane, RecoverySplitNode, RecoveryTab, RecoveryTopology, RecoveryWindow,
    SplitDirection, SplitDirectionAndSize, TerminalSize as ImageTerminalSize,
    MUX_RECOVERY_IMAGE_MAGIC, MUX_RECOVERY_IMAGE_SCHEMA_VERSION,
};
use frankenterm_core::session_restore::{
    reconstruct_whole_mux_image_inert, select_verified_recovery_roots,
    validate_whole_mux_image_layout_only, ReconstructedPaneTerminal, ReconstructedWholeMuxSession,
    ValidatedWholeMuxRecovery, WholeMuxRecoveryError, WholeMuxRecoveryLimits,
    WholeMuxRecoveryVerifier,
};
use frankenterm_core::snapshot_publication::{
    GenerationRootPublishRequest, PredecessorBinding, PublicationError, PublicationLimits,
    RecoveryObjectPayload, RootSlot, RootSlotCandidate, RootVerifier,
    SnapshotPublicationStore,
};
use frankenterm_core::snapshot_repair::{
    decode_repair_symbols_default, encode_repair_envelope_default, AuthenticatedRepairSymbol,
    RepairAdmissionController, RepairError, RepairProtectionClass,
};
use frankenterm_core::snapshot_representation::{
    decode_recovery_object, encode_recovery_object, EncryptedRecoveryObject, ExpectedContext,
    ObjectMetadata, RecoveryKey, RecoveryObjectKind, RepresentationConfig, RepresentationError,
};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::config::TerminalConfiguration;
use frankenterm_term::terminal::{InertTerminal, RecoveryTerminalCheckpointV2, Terminal};
use frankenterm_term::terminalstate::checkpoint::{
    TerminalCheckpointLimits, TerminalCheckpointV2,
};
use frankenterm_term::TerminalSize as TermTerminalSize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

// =============================================================================
// Test Configuration & Helpers
// =============================================================================

#[derive(Debug)]
struct TestTermConfig;

impl TerminalConfiguration for TestTermConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Helper to create and populate a real `frankenterm_term::Terminal` with distinct semantic contents.
fn create_live_terminal(pane_id: usize, content_script: &[&[u8]]) -> Terminal {
    let size = TermTerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 640,
        pixel_height: 384,
        dpi: 96,
    };
    let mut term = Terminal::new(
        size,
        Arc::new(TestTermConfig),
        "FrankenTerm",
        &format!("recovery-pane-{pane_id}"),
        Box::new(Vec::<u8>::new()),
    );
    for chunk in content_script {
        term.advance_bytes(chunk);
    }
    term
}

/// Helper to capture and canonically encode a terminal checkpoint using the public API.
fn capture_canonical_checkpoint(
    term: &Terminal,
    limits: TerminalCheckpointLimits,
) -> (Vec<u8>, [u8; 32], usize, usize) {
    let checkpoint: RecoveryTerminalCheckpointV2 = term
        .capture_recovery_checkpoint(limits)
        .expect("must capture canonical recovery checkpoint");
    let payload = checkpoint.canonical_payload().to_vec();
    let rows = checkpoint.rows();
    let cols = checkpoint.cols();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&Sha256::digest(&payload));
    (payload, digest, rows, cols)
}

/// Helper to assemble a complete 8-pane `MuxRecoveryImage` covering splits, floating panes,
/// multiple windows, tabs, and domains.
fn build_test_whole_mux_image(
    canonical_checkpoints: &[(Vec<u8>, [u8; 32], usize, usize)],
    inc_id: &str,
    session_id: &str,
) -> MuxRecoveryImage {
    assert_eq!(canonical_checkpoints.len(), 8);

    let header = RecoveryImageHeader {
        magic: MUX_RECOVERY_IMAGE_MAGIC,
        schema_version: MUX_RECOVERY_IMAGE_SCHEMA_VERSION,
        generation: 1,
        predecessor_digest: None,
        created_at_epoch_ms: 1700000000000,
        mux_incarnation_id: inc_id.to_string(),
        ft_version: "0.1.0".to_string(),
        session_id: session_id.to_string(),
    };

    let domains = vec![
        RecoveryDomain {
            incarnation_domain_id: 1,
            domain_name: "local".to_string(),
            is_attached: true,
        },
        RecoveryDomain {
            incarnation_domain_id: 2,
            domain_name: "remote".to_string(),
            is_attached: true,
        },
    ];

    let split_size = ImageTerminalSize {
        rows: 24,
        cols: 40,
        pixel_width: 320,
        pixel_height: 384,
        dpi: 96,
    };
    let full_size = ImageTerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 640,
        pixel_height: 384,
        dpi: 96,
    };
    let vert_split_size = ImageTerminalSize {
        rows: 12,
        cols: 80,
        pixel_width: 640,
        pixel_height: 192,
        dpi: 96,
    };

    let windows = vec![
        RecoveryWindow {
            window_id: 1,
            stable_window_id: "win-1-stable".to_string(),
            workspace: "default".to_string(),
            order_revision: 1,
            gui_position: Some(RecoveryGuiPosition {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }),
            tabs: vec![
                RecoveryTab {
                    tab_id: 10,
                    stable_tab_id: "tab-10-stable".to_string(),
                    title: "Orchestrator Leader".to_string(),
                    working_dir: Some("/workspace/leader".to_string()),
                    size: full_size,
                    size_before_zoom: full_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Split {
                        split: SplitDirectionAndSize {
                            direction: SplitDirection::Horizontal,
                            first: split_size,
                            second: split_size,
                        },
                        left: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 0,
                            pane_uuid: "uuid-recovery-pane-0".to_string(),
                        }),
                        right: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 1,
                            pane_uuid: "uuid-recovery-pane-1".to_string(),
                        }),
                    }),
                    floating_panes: Vec::new(),
                    floating_focus: None,
                    active_pane_id: 0,
                },
                RecoveryTab {
                    tab_id: 11,
                    stable_tab_id: "tab-11-stable".to_string(),
                    title: "Monitor & Scratchpad".to_string(),
                    working_dir: Some("/workspace/monitor".to_string()),
                    size: full_size,
                    size_before_zoom: full_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Leaf {
                        pane_id: 2,
                        pane_uuid: "uuid-recovery-pane-2".to_string(),
                    }),
                    floating_panes: vec![RecoveryFloatingPane::new(
                        7,
                        "uuid-recovery-pane-7".to_string(),
                        FloatingPaneRect {
                            left: 10,
                            top: 10,
                            width: 30,
                            height: 10,
                        },
                        1,
                        true,
                        false,
                        1.0,
                    )],
                    floating_focus: Some(7),
                    active_pane_id: 2,
                },
            ],
            active_tab_index: 0,
        },
        RecoveryWindow {
            window_id: 2,
            stable_window_id: "win-2-stable".to_string(),
            workspace: "default".to_string(),
            order_revision: 2,
            gui_position: Some(RecoveryGuiPosition {
                x: 100,
                y: 100,
                width: 800,
                height: 600,
            }),
            tabs: vec![
                RecoveryTab {
                    tab_id: 12,
                    stable_tab_id: "tab-12-stable".to_string(),
                    title: "Remote Agents".to_string(),
                    working_dir: Some("/workspace/remote".to_string()),
                    size: full_size,
                    size_before_zoom: full_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Split {
                        split: SplitDirectionAndSize {
                            direction: SplitDirection::Vertical,
                            first: vert_split_size,
                            second: vert_split_size,
                        },
                        left: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 3,
                            pane_uuid: "uuid-recovery-pane-3".to_string(),
                        }),
                        right: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 4,
                            pane_uuid: "uuid-recovery-pane-4".to_string(),
                        }),
                    }),
                    floating_panes: Vec::new(),
                    floating_focus: None,
                    active_pane_id: 3,
                },
                RecoveryTab {
                    tab_id: 13,
                    stable_tab_id: "tab-13-stable".to_string(),
                    title: "Build Pipeline".to_string(),
                    working_dir: Some("/workspace/build".to_string()),
                    size: full_size,
                    size_before_zoom: full_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Leaf {
                        pane_id: 5,
                        pane_uuid: "uuid-recovery-pane-5".to_string(),
                    }),
                    floating_panes: Vec::new(),
                    floating_focus: None,
                    active_pane_id: 5,
                },
                RecoveryTab {
                    tab_id: 14,
                    stable_tab_id: "tab-14-stable".to_string(),
                    title: "Telemetry Logs".to_string(),
                    working_dir: Some("/workspace/telemetry".to_string()),
                    size: full_size,
                    size_before_zoom: full_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Leaf {
                        pane_id: 6,
                        pane_uuid: "uuid-recovery-pane-6".to_string(),
                    }),
                    floating_panes: Vec::new(),
                    floating_focus: None,
                    active_pane_id: 6,
                },
            ],
            active_tab_index: 0,
        },
    ];

    let topology = RecoveryTopology {
        domains,
        windows,
        focused_window_id: Some(1),
        client_workspace: Some(ClientWorkspaceBinding {
            client_id: "client-1".to_string(),
            active_workspace: "default".to_string(),
        }),
    };

    let titles = [
        "Orchestrator Leader",
        "Worker Pool Alpha",
        "Alternate Screen Monitor",
        "Remote Agent 1",
        "Remote Agent 2",
        "Pipeline Runner",
        "Telemetry Stream",
        "Scratchpad Inspector",
    ];

    let mut panes = Vec::with_capacity(8);
    for (i, (payload, digest, rows, cols)) in canonical_checkpoints.iter().enumerate() {
        let domain_name = if (3..=5).contains(&i) {
            "remote".to_string()
        } else {
            "local".to_string()
        };

        let alt_screen_active = i == 2;

        panes.push(RecoveryPane {
            pane_id: i,
            pane_uuid: format!("uuid-recovery-pane-{i}"),
            domain_name,
            title: titles[i].to_string(),
            cwd: Some(format!("/workspace/pane-{i}")),
            size: ImageTerminalSize {
                rows: *rows,
                cols: *cols,
                pixel_width: 800,
                pixel_height: 600,
                dpi: 96,
            },
            cursor_position: (i * 2, i * 3),
            alt_screen_active,
            checkpoint: PaneCheckpointBinding {
                topology_incarnation_id: inc_id.to_string(),
                pane_uuid: format!("uuid-recovery-pane-{i}"),
                registration_generation: 1,
                parser_capture: ParserCaptureIdentity {
                    watermark_bytes: (i as u64 + 1) * 100,
                    segment_id: i as u64 + 1,
                    parser_seqno: 1,
                },
                checkpoint_ref: RecoveryObjectRef {
                    object_id: format!("obj-pane-{i}"),
                    byte_length: payload.len() as u64,
                    payload_digest: *digest,
                    schema_version: 1,
                },
                authority: CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms: 1700000000000,
                    parser_seqno: 1,
                },
            },
        });
    }

    let mut image = MuxRecoveryImage {
        header,
        topology,
        panes,
        image_digest: [0u8; 32],
    };
    image.image_digest = image.compute_digest().expect("must compute valid image digest");
    image
}

// =============================================================================
// Positive Test Suite: Full Journey & Roundtrip
// =============================================================================

#[test]
fn test_mux_recovery_image_e2e_full_positive_journey() {
    let term_limits = TerminalCheckpointLimits::default();

    // -------------------------------------------------------------------------
    // Phase 1: Live Real Terminals (%573)
    // -------------------------------------------------------------------------
    let pane_scripts: [&[&[u8]]; 8] = [
        &[b"Pane 0: Main orchestrator ready.\r\nStarting job cluster.\r\n"],
        &[b"Pane 1: Worker pool alpha active.\r\nThread count: 64.\r\n"],
        &[b"\x1b[?1049hPane 2: Alternate screen full-screen monitor.\r\n"], // Alternate screen
        &[b"Pane 3: Remote agent cluster 1 connected via domain 2.\r\n"],
        &[b"Pane 4: Remote agent cluster 2 connected via domain 2.\r\n"],
        &[b"Pane 5: Build pipeline running test matrix.\r\n"],
        &[b"Pane 6: Telemetry log streaming at 1000 events/sec.\r\n"],
        &[b"Pane 7: Floating scratchpad notes and inspection.\r\n"],
    ];

    let live_terminals: Vec<Terminal> = pane_scripts
        .iter()
        .enumerate()
        .map(|(i, script)| create_live_terminal(i, script))
        .collect();

    // -------------------------------------------------------------------------
    // Phase 2: Capture Checkpoints & Whole-Mux Model (%573, %574)
    // -------------------------------------------------------------------------
    let canonical_checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|term| capture_canonical_checkpoint(term, term_limits))
        .collect();

    assert_eq!(canonical_checkpoints.len(), 8);
    for (i, (payload, digest, rows, cols)) in canonical_checkpoints.iter().enumerate() {
        assert!(!payload.is_empty(), "pane {i} checkpoint payload must not be empty");
        assert_eq!(*rows, 24, "pane {i} rows must be 24");
        assert_eq!(*cols, 80, "pane {i} cols must be 80");
        let computed: [u8; 32] = Sha256::digest(payload).into();
        assert_eq!(*digest, computed, "pane {i} digest must match SHA-256");
    }

    let inc_id = "inc-prod-001";
    let session_id = "session-prod-alpha";
    let mux_image = build_test_whole_mux_image(&canonical_checkpoints, inc_id, session_id);
    mux_image.validate().expect("whole-mux image must pass structural validation");

    let canonical_image_bytes = mux_image
        .to_canonical_json()
        .expect("whole-mux image must serialize to canonical JSON");

    // -------------------------------------------------------------------------
    // Phase 3: Symmetric AEAD Representation Encryption (%571)
    // -------------------------------------------------------------------------
    let recovery_key = RecoveryKey::generate().expect("must generate recovery key");

    // Encrypt all 8 terminal checkpoints
    let mut encrypted_checkpoints = Vec::new();
    for (i, (payload, _digest, _rows, _cols)) in canonical_checkpoints.iter().enumerate() {
        let mut obj_id = [0u8; 32];
        obj_id[..8].copy_from_slice(&(i as u64).to_be_bytes());
        let meta = ObjectMetadata::single(
            obj_id,
            RecoveryObjectKind::TerminalCheckpoint,
            1,
            None,
            1700000000000,
        );
        let encrypted_obj = encode_recovery_object(payload, meta, &recovery_key, None)
            .expect("must encrypt terminal checkpoint object");
        let wire_bytes = encrypted_obj.to_bytes().expect("must serialize encrypted object");
        encrypted_checkpoints.push((obj_id, wire_bytes));
    }

    // Encrypt whole-mux recovery image root manifest
    let mut root_obj_id = [0u8; 32];
    root_obj_id[0] = 0xAA;
    let root_meta = ObjectMetadata::single(
        root_obj_id,
        RecoveryObjectKind::WholeMuxImage,
        1,
        None,
        1700000000000,
    );
    let encrypted_root = encode_recovery_object(&canonical_image_bytes, root_meta, &recovery_key, None)
        .expect("must encrypt whole-mux root manifest");
    let encrypted_root_bytes = encrypted_root.to_bytes().expect("must serialize encrypted root");

    // -------------------------------------------------------------------------
    // Phase 4: RaptorQ FEC Loss Recovery (%570)
    // -------------------------------------------------------------------------
    let (repair_env, repair_symbols) =
        encode_repair_envelope_default(&encrypted_checkpoints[0].1, 1, RepairProtectionClass::Standard)
            .expect("must encode RaptorQ repair envelope");
    assert!(
        repair_symbols.len() > 2,
        "must have redundancy symbols for erasure channel"
    );
    // Simulate packet erasure: drop 2 symbols
    let symbols_with_loss = repair_symbols[2..].to_vec();
    let recovered_wire_bytes = decode_repair_symbols_default(&repair_env, &symbols_with_loss)
        .expect("RaptorQ FEC must recover encrypted object across erasure channel");
    assert_eq!(
        recovered_wire_bytes, encrypted_checkpoints[0].1,
        "FEC recovered wire bytes must bit-exactly match original encrypted representation"
    );

    // -------------------------------------------------------------------------
    // Phase 5: Authenticated Representation Decryption & Verification (%571)
    // -------------------------------------------------------------------------
    // Decrypt and verify root manifest with real key and expected identity
    let parsed_enc_root = EncryptedRecoveryObject::from_bytes(&encrypted_root_bytes)
        .expect("must parse encrypted root");
    let expected_root_context = ExpectedContext::single(
        root_obj_id,
        RecoveryObjectKind::WholeMuxImage,
        1,
        None,
    );
    let decrypted_root = decode_recovery_object(
        &parsed_enc_root,
        &expected_root_context,
        &recovery_key,
        None,
    )
    .expect("must decrypt authenticated root manifest");
    assert_eq!(
        decrypted_root.plaintext(),
        &canonical_image_bytes[..],
        "decrypted root manifest must match original canonical image JSON"
    );

    // Decrypt and verify all 8 terminal checkpoints
    let mut decrypted_checkpoints = Vec::new();
    for (i, (obj_id, wire_bytes)) in encrypted_checkpoints.iter().enumerate() {
        let parsed_obj = EncryptedRecoveryObject::from_bytes(wire_bytes)
            .expect("must parse encrypted checkpoint object");
        let expected_ctx = ExpectedContext::single(
            *obj_id,
            RecoveryObjectKind::TerminalCheckpoint,
            1,
            None,
        );
        let decrypted_obj = decode_recovery_object(&parsed_obj, &expected_ctx, &recovery_key, None)
            .expect("must decrypt terminal checkpoint object with real key and expected identity");
        assert_eq!(
            decrypted_obj.plaintext(),
            &canonical_checkpoints[i].0[..],
            "decrypted terminal checkpoint must match original canonical JSON"
        );
        decrypted_checkpoints.push(decrypted_obj.into_plaintext());
    }

    // -------------------------------------------------------------------------
    // Phase 6: Snapshot Publication Store & Production Root Verifier (%572 + %576)
    // -------------------------------------------------------------------------
    let temp_store_dir = TempDir::new().expect("must create temp store directory");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("must open snapshot publication store");

    // Also persist the encrypted representation objects to the store
    for (i, (_obj_id, wire_bytes)) in encrypted_checkpoints.iter().enumerate() {
        let enc_sha = hex::encode(Sha256::digest(wire_bytes));
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("enc-obj-pane-{i}"),
                expected_sha256: enc_sha,
                ciphertext_bytes: wire_bytes.clone(),
            })
            .expect("must publish encrypted checkpoint object");
    }
    let enc_root_sha = hex::encode(Sha256::digest(&encrypted_root_bytes));
    store
        .publish_object(&RecoveryObjectPayload {
            object_id: "enc-root-manifest-gen1".to_string(),
            expected_sha256: enc_root_sha,
            ciphertext_bytes: encrypted_root_bytes.clone(),
        })
        .expect("must publish encrypted root manifest");

    // Publish canonical terminal checkpoint objects for root closure
    for (i, decrypted_payload) in decrypted_checkpoints.iter().enumerate() {
        let digest = canonical_checkpoints[i].1;
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{}", i),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: decrypted_payload.to_vec(),
            })
            .expect("must publish canonical checkpoint object");
    }

    // Publish generation 1 root using the production WholeMuxRecoveryVerifier
    let prod_verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let publish_receipt = store
        .publish_generation_root(
            &GenerationRootPublishRequest {
                generation: 1,
                predecessor: None,
                publisher_id: "test-recovery-e2e".to_string(),
                manifest_bytes: decrypted_root.plaintext().to_vec(),
            },
            &prod_verifier,
        )
        .expect("production WholeMuxRecoveryVerifier must admit generation 1 root");

    assert_eq!(publish_receipt.generation, 1);
    assert_eq!(publish_receipt.slot, RootSlot::SlotA);

    // Select verified recovery roots from store using production verifier
    let verified_selection = select_verified_recovery_roots(&store, &prod_verifier)
        .expect("select_verified_recovery_roots must succeed");
    let active_root = verified_selection.active_root.expect("active root must be present");
    assert_eq!(active_root.generation, 1);
    let validated_recovery = active_root.verified;
    assert_eq!(validated_recovery.checkpoint_payloads.len(), 8);

    // -------------------------------------------------------------------------
    // Phase 7: Inert Whole-Mux Session Reconstruction (%576)
    // -------------------------------------------------------------------------
    let active_live_sessions = HashSet::from(["fleet-active-target".to_string()]);
    let reconstructed = reconstruct_whole_mux_image_inert(
        &validated_recovery,
        term_limits,
        Some("offline-recovery-target"),
        &active_live_sessions,
    )
    .expect("reconstruct_whole_mux_image_inert must reconstruct all 8 inert terminals");

    // -------------------------------------------------------------------------
    // Phase 8: Semantic & Topology Assertions
    // -------------------------------------------------------------------------
    assert_eq!(reconstructed.header.session_id, session_id);
    assert_eq!(reconstructed.header.mux_incarnation_id, inc_id);
    assert_eq!(reconstructed.topology.windows.len(), 2);
    assert_eq!(reconstructed.topology.domains.len(), 2);
    assert_eq!(reconstructed.pane_terminals.len(), 8);

    // Verify non-zero tab_id and domain_id bindings for every reconstructed pane
    for pane_id in 0..8u64 {
        let pt = reconstructed
            .pane_terminals
            .get(&pane_id)
            .unwrap_or_else(|| panic!("pane {pane_id} must be reconstructed"));
        assert_eq!(pt.pane_id, pane_id);
        assert!(
            pt.tab_id >= 10,
            "pane {pane_id} tab_id must be non-zero (found {})",
            pt.tab_id
        );
        assert!(
            pt.domain_id >= 1,
            "pane {pane_id} domain_id must be non-zero (found {})",
            pt.domain_id
        );
        assert_eq!(pt.rows, 24, "pane {pane_id} rows must be 24");
        assert_eq!(pt.cols, 80, "pane {pane_id} cols must be 80");
    }

    // Specific tab bindings
    assert_eq!(reconstructed.pane_terminals[&0].tab_id, 10);
    assert_eq!(reconstructed.pane_terminals[&1].tab_id, 10);
    assert_eq!(reconstructed.pane_terminals[&2].tab_id, 11);
    assert_eq!(reconstructed.pane_terminals[&7].tab_id, 11); // Floating pane
    assert_eq!(reconstructed.pane_terminals[&3].tab_id, 12);
    assert_eq!(reconstructed.pane_terminals[&4].tab_id, 12);
    assert_eq!(reconstructed.pane_terminals[&5].tab_id, 13);
    assert_eq!(reconstructed.pane_terminals[&6].tab_id, 14);

    // Specific domain bindings
    assert_eq!(reconstructed.pane_terminals[&0].domain_id, 1); // local
    assert_eq!(reconstructed.pane_terminals[&1].domain_id, 1); // local
    assert_eq!(reconstructed.pane_terminals[&2].domain_id, 1); // local
    assert_eq!(reconstructed.pane_terminals[&7].domain_id, 1); // local
    assert_eq!(reconstructed.pane_terminals[&3].domain_id, 2); // remote
    assert_eq!(reconstructed.pane_terminals[&4].domain_id, 2); // remote
    assert_eq!(reconstructed.pane_terminals[&5].domain_id, 2); // remote
    assert_eq!(reconstructed.pane_terminals[&6].domain_id, 1); // local

    // Verify alternate screen flag on Pane 2 vs other panes
    assert!(
        validated_recovery.image.panes[2].alt_screen_active,
        "pane 2 must have alternate screen active"
    );
    for (i, p) in validated_recovery.image.panes.iter().enumerate() {
        if i != 2 {
            assert!(!p.alt_screen_active, "pane {i} must not have alternate screen active");
        }
    }
}

// =============================================================================
// Negative Test Suite: Causal Defect Matrix
// =============================================================================

#[test]
fn test_mux_recovery_e2e_fec_insufficient_rank_fails_closed() {
    let payload = b"critical-terminal-payload-requiring-full-rank";
    let (repair_env, repair_symbols) =
        encode_repair_envelope_default(payload, 1, RepairProtectionClass::Standard)
            .expect("must encode repair envelope");

    // Pass only 1 symbol when more are required for complete rank
    let insufficient_symbols = vec![repair_symbols[0].clone()];
    let result = decode_repair_symbols_default(&repair_env, &insufficient_symbols);

    assert!(
        matches!(result, Err(RepairError::InsufficientRank { .. })),
        "FEC must fail closed with InsufficientRank when symbol count is inadequate"
    );
}

#[test]
fn test_mux_recovery_e2e_ciphertext_tamper_fails_closed() {
    let payload = b"confidential-state-buffer-to-protect";
    let key = RecoveryKey::generate().expect("key");
    let meta = ObjectMetadata::single([1u8; 32], RecoveryObjectKind::TerminalCheckpoint, 1, None, 100);

    let encrypted = encode_recovery_object(payload, meta.clone(), &key, None).expect("encrypt");
    let mut wire_bytes = encrypted.to_bytes().expect("to_bytes");

    // Tamper with the last byte of the wire payload
    let last = wire_bytes.len() - 1;
    wire_bytes[last] ^= 0xFF;

    let parsed_result = EncryptedRecoveryObject::from_bytes(&wire_bytes);
    match parsed_result {
        Ok(tampered_obj) => {
            let expected = ExpectedContext::from_metadata(&meta);
            let decode_result = decode_recovery_object(&tampered_obj, &expected, &key, None);
            assert!(
                decode_result.is_err(),
                "AEAD decode must fail closed on tampered ciphertext"
            );
        }
        Err(err) => {
            // Rejection during envelope parse (e.g. digest mismatch) is also valid fail-closed behavior
            assert!(
                matches!(err, RepresentationError::DigestMismatch { .. } | RepresentationError::MalformedRepresentation { .. }),
                "parsing tampered wire bytes must fail closed with RepresentationError"
            );
        }
    }
}

#[test]
fn test_mux_recovery_e2e_wrong_key_fails_closed() {
    let payload = b"confidential-state-for-key-authorization-test";
    let key_correct = RecoveryKey::generate().expect("key 1");
    let key_wrong = RecoveryKey::generate().expect("key 2");
    let meta = ObjectMetadata::single([2u8; 32], RecoveryObjectKind::TerminalCheckpoint, 1, None, 100);

    let encrypted = encode_recovery_object(payload, meta.clone(), &key_correct, None).expect("encrypt");
    let expected = ExpectedContext::from_metadata(&meta);

    let result = decode_recovery_object(&encrypted, &expected, &key_wrong, None);
    assert!(
        result.is_err(),
        "decoding with wrong RecoveryKey must fail closed"
    );
}

#[test]
fn test_mux_recovery_e2e_torn_generation_falls_back_to_intact_predecessor() {
    let term_limits = TerminalCheckpointLimits::default();
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let live_terminals: Vec<Terminal> = (0..8)
        .map(|i| create_live_terminal(i, &[format!("Pane {i}\r\n").as_bytes()]))
        .collect();

    let checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|t| capture_canonical_checkpoint(t, term_limits))
        .collect();

    // Publish Generation 1 objects
    for (i, (payload, digest, _, _)) in checkpoints.iter().enumerate() {
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{i}"),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: payload.clone(),
            })
            .expect("publish object");
    }

    let image_gen1 = build_test_whole_mux_image(&checkpoints, "inc-1", "sess-1");
    let gen1_bytes = image_gen1.to_canonical_json().expect("gen1 json");
    let prod_verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());

    let r1 = store
        .publish_generation_root(
            &GenerationRootPublishRequest {
                generation: 1,
                predecessor: None,
                publisher_id: "publisher-1".to_string(),
                manifest_bytes: gen1_bytes.clone(),
            },
            &prod_verifier,
        )
        .expect("publish generation 1");

    assert_eq!(r1.generation, 1);
    assert_eq!(r1.slot, RootSlot::SlotA);

    // Count store directory entries before torn publish
    let dir_entries_before = std::fs::read_dir(temp_store_dir.path())
        .expect("read_dir")
        .count();

    // Attempt to publish Generation 2 which references a missing object "obj-pane-999"
    let mut image_gen2 = image_gen1.clone();
    image_gen2.header.generation = 2;
    image_gen2.header.predecessor_digest = Some(image_gen1.image_digest);
    image_gen2.panes[0].checkpoint.checkpoint_ref.object_id = "obj-pane-999".to_string();
    image_gen2.image_digest = image_gen2.compute_digest().expect("gen2 digest");
    let gen2_bytes = image_gen2.to_canonical_json().expect("gen2 json");

    let gen1_sha256 = hex::encode(Sha256::digest(&gen1_bytes));
    let gen2_req = GenerationRootPublishRequest {
        generation: 2,
        predecessor: Some(PredecessorBinding {
            expected_generation: 1,
            expected_hash: gen1_sha256,
        }),
        publisher_id: "publisher-1".to_string(),
        manifest_bytes: gen2_bytes,
    };

    let gen2_result = store.publish_generation_root(&gen2_req, &prod_verifier);
    assert!(
        gen2_result.is_err(),
        "publishing generation 2 with missing object must fail closed"
    );

    // Rule 1 / Zero-Deletion Invariant: No files were deleted during torn publish failure
    let dir_entries_after = std::fs::read_dir(temp_store_dir.path())
        .expect("read_dir")
        .count();
    assert!(
        dir_entries_after >= dir_entries_before,
        "failed publication must never delete any files"
    );

    // Dual-slot selection falls back cleanly to Generation 1
    let selection = select_verified_recovery_roots(&store, &prod_verifier)
        .expect("select verified roots");
    let active = selection.active_root.expect("must have active root");
    assert_eq!(
        active.generation, 1,
        "selection must fall back to intact generation 1 predecessor"
    );
    assert_eq!(active.slot, RootSlot::SlotA);
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_tampered_object() {
    let term_limits = TerminalCheckpointLimits::default();
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let live_terminals: Vec<Terminal> = (0..8)
        .map(|i| create_live_terminal(i, &[format!("Pane {i}\r\n").as_bytes()]))
        .collect();

    let checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|t| capture_canonical_checkpoint(t, term_limits))
        .collect();

    let image = build_test_whole_mux_image(&checkpoints, "inc-1", "sess-1");
    let image_bytes = image.to_canonical_json().expect("json");

    // Publish 7 valid objects, but tamper with object 0
    for (i, (payload, digest, _, _)) in checkpoints.iter().enumerate() {
        let (upload_payload, upload_sha) = if i == 0 {
            let mut tampered = payload.clone();
            let last = tampered.len() - 1;
            tampered[last] ^= 0x01; // flip 1 bit
            let sha = hex::encode(Sha256::digest(&tampered));
            (tampered, sha)
        } else {
            (payload.clone(), hex::encode(digest))
        };

        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{i}"),
                expected_sha256: upload_sha,
                ciphertext_bytes: upload_payload,
            })
            .expect("publish object");
    }

    let candidate = RootSlotCandidate {
        slot: RootSlot::SlotA,
        generation: 1,
        publisher_id: "test-pub".to_string(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: hex::encode(Sha256::digest(&image_bytes)),
        manifest_bytes: image_bytes,
        file_len: 1000,
        created_at_ms: 1700000000000,
    };

    let verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let result = verifier.verify_root(&candidate, &store);

    assert!(
        matches!(result, Err(WholeMuxRecoveryError::CheckpointDigestMismatch { .. })),
        "WholeMuxRecoveryVerifier must reject candidate with CheckpointDigestMismatch on tampered payload"
    );
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_missing_object() {
    let term_limits = TerminalCheckpointLimits::default();
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let live_terminals: Vec<Terminal> = (0..8)
        .map(|i| create_live_terminal(i, &[format!("Pane {i}\r\n").as_bytes()]))
        .collect();

    let checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|t| capture_canonical_checkpoint(t, term_limits))
        .collect();

    let image = build_test_whole_mux_image(&checkpoints, "inc-1", "sess-1");
    let image_bytes = image.to_canonical_json().expect("json");

    // Intentionally omit publishing object 3
    for (i, (payload, digest, _, _)) in checkpoints.iter().enumerate() {
        if i == 3 {
            continue;
        }
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{i}"),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: payload.clone(),
            })
            .expect("publish object");
    }

    let candidate = RootSlotCandidate {
        slot: RootSlot::SlotA,
        generation: 1,
        publisher_id: "test-pub".to_string(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: hex::encode(Sha256::digest(&image_bytes)),
        manifest_bytes: image_bytes,
        file_len: 1000,
        created_at_ms: 1700000000000,
    };

    let verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let result = verifier.verify_root(&candidate, &store);

    assert!(
        matches!(result, Err(WholeMuxRecoveryError::MissingCheckpointObject(ref id)) if id == "obj-pane-3"),
        "WholeMuxRecoveryVerifier must reject candidate with MissingCheckpointObject for missing object"
    );
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_fake_guardian() {
    let term_limits = TerminalCheckpointLimits::default();
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let live_terminals: Vec<Terminal> = (0..8)
        .map(|i| create_live_terminal(i, &[format!("Pane {i}\r\n").as_bytes()]))
        .collect();

    let checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|t| capture_canonical_checkpoint(t, term_limits))
        .collect();

    for (i, (payload, digest, _, _)) in checkpoints.iter().enumerate() {
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{i}"),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: payload.clone(),
            })
            .expect("publish object");
    }

    let mut image = build_test_whole_mux_image(&checkpoints, "inc-1", "sess-1");

    // Plant fake guardian authority claim: Guardian authority but watermark_bytes = 0
    image.panes[1].checkpoint.authority = CheckpointAuthority::Guardian {
        durable_pane_id: "pane-uuid-1".to_string(),
        segment_id: 1,
        watermark_bytes: 0,
        registration_wire_identity: [1u8; 16],
        output_append_receipt_sha256: [0u8; 32],
        terminal_replay_semantics_id: "test".to_string(),
        lease_verifier: "verifier".to_string(),
        guardian_generation: 1,
        catalog_generation: 1,
    };
    image.image_digest = image.compute_digest().expect("digest");
    let image_bytes = image.to_canonical_json().expect("json");

    let candidate = RootSlotCandidate {
        slot: RootSlot::SlotA,
        generation: 1,
        publisher_id: "test-pub".to_string(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: hex::encode(Sha256::digest(&image_bytes)),
        manifest_bytes: image_bytes,
        file_len: 1000,
        created_at_ms: 1700000000000,
    };

    let verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let result = verifier.verify_root(&candidate, &store);

    assert!(
        matches!(result, Err(WholeMuxRecoveryError::FakeGuardianAuthority { pane_id: 1 })),
        "WholeMuxRecoveryVerifier must reject candidate with FakeGuardianAuthority when watermark is zero"
    );
}

#[test]
fn test_mux_recovery_e2e_production_verifier_rejects_corrupted_manifest() {
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let garbage_bytes = b"NOT-A-CANONICAL-JSON-RECOVERY-IMAGE-MANIFEST".to_vec();
    let candidate = RootSlotCandidate {
        slot: RootSlot::SlotA,
        generation: 1,
        publisher_id: "test-pub".to_string(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: hex::encode(Sha256::digest(&garbage_bytes)),
        manifest_bytes: garbage_bytes,
        file_len: 100,
        created_at_ms: 1700000000000,
    };

    let verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let result = verifier.verify_root(&candidate, &store);

    assert!(
        matches!(result, Err(WholeMuxRecoveryError::Image(_))),
        "WholeMuxRecoveryVerifier must reject candidate with Image deserialization error on garbage manifest"
    );
}

#[test]
fn test_mux_recovery_e2e_reconstruct_refuses_active_live_destination() {
    let term_limits = TerminalCheckpointLimits::default();
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let live_terminals: Vec<Terminal> = (0..8)
        .map(|i| create_live_terminal(i, &[format!("Pane {i}\r\n").as_bytes()]))
        .collect();

    let checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|t| capture_canonical_checkpoint(t, term_limits))
        .collect();

    for (i, (payload, digest, _, _)) in checkpoints.iter().enumerate() {
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{i}"),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: payload.clone(),
            })
            .expect("publish object");
    }

    let image = build_test_whole_mux_image(&checkpoints, "inc-1", "sess-1");
    let image_bytes = image.to_canonical_json().expect("json");

    let candidate = RootSlotCandidate {
        slot: RootSlot::SlotA,
        generation: 1,
        publisher_id: "test-pub".to_string(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: hex::encode(Sha256::digest(&image_bytes)),
        manifest_bytes: image_bytes,
        file_len: 1000,
        created_at_ms: 1700000000000,
    };

    let verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let validated = verifier.verify_root(&candidate, &store).expect("root verification must succeed");

    // Destination namespace is active in fleet
    let active_sessions = HashSet::from([
        "active-fleet-namespace".to_string(),
        "live-session-2".to_string(),
    ]);

    let result = reconstruct_whole_mux_image_inert(
        &validated,
        term_limits,
        Some("active-fleet-namespace"),
        &active_sessions,
    );

    assert!(
        matches!(result, Err(WholeMuxRecoveryError::LiveDestinationRefused { ref namespace }) if namespace == "active-fleet-namespace"),
        "reconstruct_whole_mux_image_inert must fail closed with LiveDestinationRefused when target is active"
    );
}

#[test]
fn test_mux_recovery_e2e_layout_only_validation() {
    let term_limits = TerminalCheckpointLimits::default();
    let temp_store_dir = TempDir::new().expect("temp store");
    let store = SnapshotPublicationStore::open(
        temp_store_dir.path(),
        PublicationLimits {
            max_root_manifest_bytes: 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_dir_entries: 1024,
            max_error_records: 64,
        },
    )
    .expect("open store");

    let live_terminals: Vec<Terminal> = (0..8)
        .map(|i| create_live_terminal(i, &[format!("Pane {i}\r\n").as_bytes()]))
        .collect();

    let checkpoints: Vec<(Vec<u8>, [u8; 32], usize, usize)> = live_terminals
        .iter()
        .map(|t| capture_canonical_checkpoint(t, term_limits))
        .collect();

    for (i, (payload, digest, _, _)) in checkpoints.iter().enumerate() {
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: format!("obj-pane-{i}"),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: payload.clone(),
            })
            .expect("publish object");
    }

    let image = build_test_whole_mux_image(&checkpoints, "inc-1", "sess-1");
    let image_bytes = image.to_canonical_json().expect("json");

    let candidate = RootSlotCandidate {
        slot: RootSlot::SlotA,
        generation: 1,
        publisher_id: "test-pub".to_string(),
        predecessor_generation: None,
        predecessor_hash: None,
        manifest_sha256: hex::encode(Sha256::digest(&image_bytes)),
        manifest_bytes: image_bytes,
        file_len: 1000,
        created_at_ms: 1700000000000,
    };

    let verifier = WholeMuxRecoveryVerifier::new(WholeMuxRecoveryLimits::default());
    let validated = verifier.verify_root(&candidate, &store).expect("root verification must succeed");

    let active_sessions = HashSet::from(["busy-fleet-namespace".to_string()]);

    // Rejection on active live destination
    let rejected = validate_whole_mux_image_layout_only(
        &validated,
        Some("busy-fleet-namespace"),
        &active_sessions,
    );
    assert!(
        matches!(rejected, Err(WholeMuxRecoveryError::LiveDestinationRefused { .. })),
        "layout-only validation must refuse active live destination"
    );

    // Acceptance on safe offline destination
    let accepted = validate_whole_mux_image_layout_only(
        &validated,
        Some("offline-safe-namespace"),
        &active_sessions,
    );
    assert!(
        accepted.is_ok(),
        "layout-only validation must accept safe offline destination"
    );
}
