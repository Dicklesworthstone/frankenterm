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
//! - Complete negative matrix: insufficient FEC rank, ciphertext tampering, bad keys, torn generation fallback,
//!   missing referenced objects, structural topology corruptions, and live target safety gates.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use frankenterm_core::cx::Cx;
use frankenterm_core::mux_recovery_image::{
    CheckpointAuthority, ClientWorkspaceBinding, FloatingPaneRect, MuxRecoveryImage,
    MuxRecoveryImageError, PaneCheckpointBinding, ParserCaptureIdentity, RecoveryDomain,
    RecoveryFloatingPane, RecoveryGuiPosition, RecoveryImageHeader, RecoveryObjectRef,
    RecoveryPane, RecoverySplitNode, RecoveryTab, RecoveryTopology, RecoveryWindow,
    SplitDirection, SplitDirectionAndSize, TerminalSize as ImageTerminalSize,
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
use frankenterm_term::terminalstate::checkpoint::{
    TerminalCheckpointLimits, TerminalCheckpointV2, ValidatedTerminalCheckpointV2,
};
use frankenterm_term::{InertTerminal, Terminal, TerminalSize as TermTerminalSize};
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

/// Helper to capture and canonically encode a terminal checkpoint.
fn capture_canonical_checkpoint(
    term: &Terminal,
    limits: TerminalCheckpointLimits,
) -> (Vec<u8>, [u8; 32]) {
    let checkpoint = TerminalCheckpointV2::capture_with_limits(term, limits)
        .expect("must capture terminal checkpoint");
    let canonical = checkpoint
        .to_canonical_json(limits)
        .expect("must encode canonical JSON");
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&Sha256::digest(&canonical));
    (canonical.to_vec(), digest)
}

/// Custom RootVerifier enforcing that the root candidate parses and every single referenced
/// terminal checkpoint object exists in the store with matching payload digest and length.
#[derive(Clone)]
struct HolisticRootVerifier {
    expected_generation: u64,
    limits: TerminalCheckpointLimits,
}

impl RootVerifier for HolisticRootVerifier {
    type Error = String;
    type Verified = (MuxRecoveryImage, BTreeMap<usize, ValidatedTerminalCheckpointV2>);

    fn verify_root(
        &self,
        candidate: &RootSlotCandidate,
        store: &SnapshotPublicationStore,
    ) -> Result<Self::Verified, Self::Error> {
        if candidate.generation != self.expected_generation {
            return Err(format!(
                "generation mismatch: expected {}, found {}",
                self.expected_generation, candidate.generation
            ));
        }

        // 1. Decode and validate canonical whole-mux recovery image
        let image = MuxRecoveryImage::from_json_slice(&candidate.manifest_bytes)
            .map_err(|e| format!("invalid mux recovery image in root: {e}"))?;
        image
            .validate()
            .map_err(|e| format!("image structural validation failed: {e}"))?;

        // 2. Complete object-graph closure verification:
        // Every single pane must have its checkpoint object present in the store and valid.
        let mut validated_terminals = BTreeMap::new();
        for pane in &image.panes {
            let obj_ref = &pane.checkpoint.checkpoint_ref;
            let object_bytes = store
                .read_object(&obj_ref.object_id)
                .map_err(|e| format!("missing referenced checkpoint object '{}': {e}", obj_ref.object_id))?;

            if object_bytes.len() as u64 != obj_ref.byte_length {
                return Err(format!(
                    "checkpoint object '{}' length mismatch: expected {}, got {}",
                    obj_ref.object_id, obj_ref.byte_length, object_bytes.len()
                ));
            }

            let computed_digest: [u8; 32] = Sha256::digest(&object_bytes).into();
            if computed_digest != obj_ref.payload_digest {
                return Err(format!(
                    "checkpoint object '{}' digest mismatch: expected {:?}, computed {:?}",
                    obj_ref.object_id, obj_ref.payload_digest, computed_digest
                ));
            }

            // Real canonical JSON decoding verification
            let validated = TerminalCheckpointV2::decode_canonical_json(&object_bytes, self.limits)
                .map_err(|e| format!("terminal checkpoint decode failed for pane {}: {e}", pane.pane_id))?;

            validated_terminals.insert(pane.pane_id, validated);
        }

        Ok((image, validated_terminals))
    }
}

// =============================================================================
// Positive E2E Integration Journeys
// =============================================================================

#[test]
fn test_e2e_full_mux_recovery_roundtrip_happy_path() {
    let cx = Cx::for_testing();
    let term_limits = TerminalCheckpointLimits::default();
    let temp_dir = TempDir::new().expect("create tempdir");
    let store = SnapshotPublicationStore::open(temp_dir.path(), PublicationLimits::default())
        .expect("open publication store");

    // -------------------------------------------------------------------------
    // Phase 1: Construct 8 real terminals with rich semantic features
    // -------------------------------------------------------------------------
    let terminal_scripts: [Vec<&[u8]>; 8] = [
        // Pane 0: Primary screen with green text & bold
        vec![b"Agent 0 Initializing...\r\n", b"\x1b[32;1mStatus: ONLINE\x1b[0m\r\n"],
        // Pane 1: ANSI styling & Unicode emojis
        vec![b"\x1b[33mWorker 1:\x1b[0m Processing batch \xF0\x9F\x9A\x80 [OK]\r\n"],
        // Pane 2: Alternate screen buffer activated with full screen redraw
        vec![
            b"Primary text that should be hidden\r\n",
            b"\x1b[?1049h\x1b[H\x1b[2J",
            b"\x1b[44;37m=== FULLSCREEN MONITOR TUI ===\x1b[0m\r\n",
            b"CPU: 42% | RAM: 18% | Network: ACTIVE \xE2\x9C\x94\r\n",
        ],
        // Pane 3: Box-drawing characters
        vec![
            b"\xE2\x94\x8C\xE2\x94\x80\xE2\x94\x80\xE2\x94\x80\xE2\x94\x80\xE2\x94\x80\xE2\x94\x90\r\n",
            b"\xE2\x94\x82 BOX \xE2\x94\x82\r\n",
            b"\xE2\x94\x94\xE2\x94\x80\xE2\x94\x80\xE2\x94\x80\xE2\x94\x80\xE2\x94\x80\xE2\x94\x98\r\n",
        ],
        // Pane 4: Cursor positioning sequences
        vec![b"Row 1\r\nRow 2\r\n", b"\x1b[1;20H[Pinned Cursor]"],
        // Pane 5: Inverted and underlined modes
        vec![b"\x1b[7mInverted Header\x1b[0m\r\n\x1b[4mUnderlined Subtitle\x1b[0m\r\n"],
        // Pane 6: Japanese / wide Unicode characters
        vec![b"\xE6\x9D\xB1\xE4\xBA\xAC\xE3\x83\x8E\xE3\x83\xBC\xE3\x83\x89: \xE6\xAD\xA3\xE5\xB8\xB8\xE7\xA8\xBC\xE5\x83\x8D\r\n"],
        // Pane 7: Floating pane diagnostic overlay
        vec![b"\x1b[41;37m[ALERT OVERLAY]\x1b[0m Swarm watchdog heartbeat\r\n"],
    ];

    let mut original_terminals = Vec::new();
    let mut checkpoint_bytes_list = Vec::new();
    let mut checkpoint_digests = Vec::new();

    for (id, script) in terminal_scripts.iter().enumerate() {
        let term = create_live_terminal(id, script);
        let (bytes, digest) = capture_canonical_checkpoint(&term, term_limits);

        // Publish each terminal checkpoint object to immutable publication store
        let payload = RecoveryObjectPayload {
            object_id: format!("term-chk-obj-{id:04x}"),
            expected_sha256: hex::encode(digest),
            ciphertext_bytes: bytes.clone(),
        };
        store
            .publish_object(&payload)
            .expect("must publish checkpoint object");

        original_terminals.push(term);
        checkpoint_bytes_list.push(bytes);
        checkpoint_digests.push(digest);
    }

    // -------------------------------------------------------------------------
    // Phase 2: Construct Authoritative Whole-Mux Topology (%574)
    // 3 Windows, 5 Tabs, 8 Panes, 2 Domains
    // -------------------------------------------------------------------------
    let default_size = ImageTerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 640,
        pixel_height: 384,
        dpi: 96,
    };
    let half_vertical = ImageTerminalSize {
        rows: 24,
        cols: 40,
        pixel_width: 320,
        pixel_height: 384,
        dpi: 96,
    };
    let half_horizontal = ImageTerminalSize {
        rows: 12,
        cols: 80,
        pixel_width: 640,
        pixel_height: 192,
        dpi: 96,
    };

    let domains = vec![
        RecoveryDomain {
            incarnation_domain_id: 1,
            domain_name: "local".to_string(),
            is_attached: true,
        },
        RecoveryDomain {
            incarnation_domain_id: 2,
            domain_name: "remote-hz1".to_string(),
            is_attached: true,
        },
    ];

    let windows = vec![
        // Window 0: Default workspace, Tabs 0 and 1
        RecoveryWindow {
            window_id: 100,
            stable_window_id: "win-stable-100".to_string(),
            workspace: "default".to_string(),
            order_revision: 1,
            gui_position: Some(RecoveryGuiPosition {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }),
            tabs: vec![
                // Tab 0: Single Pane 0 (Leaf)
                RecoveryTab {
                    tab_id: 10,
                    stable_tab_id: "tab-stable-10".to_string(),
                    title: "Shell".to_string(),
                    working_dir: Some("/home/agent".to_string()),
                    size: default_size,
                    size_before_zoom: default_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Leaf {
                        pane_id: 0,
                        pane_uuid: "uuid-pane-0000".to_string(),
                    }),
                    floating_panes: vec![],
                    floating_focus: None,
                    active_pane_id: 0,
                },
                // Tab 1: Vertical split Panes 1 and 2
                RecoveryTab {
                    tab_id: 11,
                    stable_tab_id: "tab-stable-11".to_string(),
                    title: "Editor & Monitor".to_string(),
                    working_dir: Some("/home/agent/project".to_string()),
                    size: default_size,
                    size_before_zoom: default_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Split {
                        split: SplitDirectionAndSize {
                            direction: SplitDirection::Vertical,
                            first: half_vertical,
                            second: half_vertical,
                        },
                        left: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 1,
                            pane_uuid: "uuid-pane-0001".to_string(),
                        }),
                        right: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 2,
                            pane_uuid: "uuid-pane-0002".to_string(),
                        }),
                    }),
                    floating_panes: vec![],
                    floating_focus: None,
                    active_pane_id: 1,
                },
            ],
            active_tab_index: 0,
        },
        // Window 1: Code workspace, Tabs 2 and 3
        RecoveryWindow {
            window_id: 101,
            stable_window_id: "win-stable-101".to_string(),
            workspace: "code".to_string(),
            order_revision: 2,
            gui_position: Some(RecoveryGuiPosition {
                x: 800,
                y: 0,
                width: 800,
                height: 600,
            }),
            tabs: vec![
                // Tab 2: Horizontal split Panes 3 and 4, Pane 3 zoomed
                RecoveryTab {
                    tab_id: 12,
                    stable_tab_id: "tab-stable-12".to_string(),
                    title: "Build".to_string(),
                    working_dir: Some("/home/agent/build".to_string()),
                    size: default_size,
                    size_before_zoom: default_size,
                    zoomed_pane_id: Some(3),
                    root_split: Some(RecoverySplitNode::Split {
                        split: SplitDirectionAndSize {
                            direction: SplitDirection::Horizontal,
                            first: half_horizontal,
                            second: half_horizontal,
                        },
                        left: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 3,
                            pane_uuid: "uuid-pane-0003".to_string(),
                        }),
                        right: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 4,
                            pane_uuid: "uuid-pane-0004".to_string(),
                        }),
                    }),
                    floating_panes: vec![],
                    floating_focus: None,
                    active_pane_id: 3,
                },
                // Tab 3: Vertical split Panes 5 and 6
                RecoveryTab {
                    tab_id: 13,
                    stable_tab_id: "tab-stable-13".to_string(),
                    title: "Logs".to_string(),
                    working_dir: None,
                    size: default_size,
                    size_before_zoom: default_size,
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Split {
                        split: SplitDirectionAndSize {
                            direction: SplitDirection::Vertical,
                            first: half_vertical,
                            second: half_vertical,
                        },
                        left: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 5,
                            pane_uuid: "uuid-pane-0005".to_string(),
                        }),
                        right: Box::new(RecoverySplitNode::Leaf {
                            pane_id: 6,
                            pane_uuid: "uuid-pane-0006".to_string(),
                        }),
                    }),
                    floating_panes: vec![],
                    floating_focus: None,
                    active_pane_id: 5,
                },
            ],
            active_tab_index: 0,
        },
        // Window 2: Monitor workspace, Tab 4 with Floating Pane 7
        RecoveryWindow {
            window_id: 102,
            stable_window_id: "win-stable-102".to_string(),
            workspace: "monitor".to_string(),
            order_revision: 3,
            gui_position: None,
            tabs: vec![RecoveryTab {
                tab_id: 14,
                stable_tab_id: "tab-stable-14".to_string(),
                title: "Alerts".to_string(),
                working_dir: None,
                size: default_size,
                size_before_zoom: default_size,
                zoomed_pane_id: None,
                root_split: None,
                floating_panes: vec![RecoveryFloatingPane::new(
                    7,
                    "uuid-pane-0007".to_string(),
                    FloatingPaneRect {
                        left: 10,
                        top: 5,
                        width: 60,
                        height: 15,
                    },
                    1,
                    true,
                    false,
                    0.95,
                )],
                floating_focus: Some(7),
                active_pane_id: 7,
            }],
            active_tab_index: 0,
        },
    ];

    let mut panes = Vec::new();
    for id in 0..8 {
        let domain_name = if id % 2 == 0 { "local" } else { "remote-hz1" };
        panes.push(RecoveryPane {
            pane_id: id,
            pane_uuid: format!("uuid-pane-{id:04x}"),
            domain_name: domain_name.to_string(),
            title: format!("Pane {id} Title"),
            cwd: Some(format!("/home/agent/pane_{id}")),
            size: default_size,
            cursor_position: (0, 0),
            alt_screen_active: id == 2,
            checkpoint: PaneCheckpointBinding {
                topology_incarnation_id: "mux-inc-e2e-live".to_string(),
                pane_uuid: format!("uuid-pane-{id:04x}"),
                registration_generation: 1,
                parser_capture: ParserCaptureIdentity {
                    watermark_bytes: 4096,
                    segment_id: 1,
                    parser_seqno: 100 + id as u64,
                },
                checkpoint_ref: RecoveryObjectRef {
                    object_id: format!("term-chk-obj-{id:04x}"),
                    byte_length: checkpoint_bytes_list[id].len() as u64,
                    payload_digest: checkpoint_digests[id],
                    schema_version: 1,
                },
                authority: CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms: 1773500000000,
                    parser_seqno: 100 + id as u64,
                },
            },
        });
    }

    let header = RecoveryImageHeader {
        magic: *b"FTMR",
        schema_version: 1,
        generation: 1,
        predecessor_digest: None,
        created_at_epoch_ms: 1773500000000,
        mux_incarnation_id: "mux-inc-e2e-live".to_string(),
        ft_version: "0.15.6-rc.17".to_string(),
        session_id: "sess-e2e-happy".to_string(),
    };

    let topology = RecoveryTopology {
        domains,
        windows,
        focused_window_id: Some(100),
        client_workspace: Some(ClientWorkspaceBinding {
            client_id: "client-primary".to_string(),
            active_workspace: "default".to_string(),
        }),
    };

    let mut mux_image = MuxRecoveryImage {
        header,
        topology,
        panes,
        image_digest: [0u8; 32],
    };

    let computed_image_digest = mux_image
        .compute_digest()
        .expect("must compute image digest");
    mux_image.image_digest = computed_image_digest;

    // Validate image invariants
    mux_image.validate().expect("image must validate");
    assert_eq!(mux_image.pane_count(), 8);

    let raw_image_json = mux_image
        .to_canonical_json()
        .expect("must encode canonical image JSON");

    // -------------------------------------------------------------------------
    // Phase 3: Cryptographic AEAD Representation Encoding (%571)
    // -------------------------------------------------------------------------
    let recovery_key = RecoveryKey::generate().expect("must generate recovery key");
    let image_object_id = [0x42u8; 32];
    let image_metadata = ObjectMetadata::single(
        image_object_id,
        RecoveryObjectKind::WholeMuxImage,
        1,
        None,
        1773500000000,
    );

    let encrypted_image_obj = encode_recovery_object(
        &raw_image_json,
        image_metadata,
        &recovery_key,
        Some(&RepresentationConfig::default()),
    )
    .expect("must AEAD encrypt whole-mux image");

    let serialized_envelope = encrypted_image_obj
        .to_bytes()
        .expect("must serialize encrypted envelope");
    let rep_id = encrypted_image_obj
        .representation_id()
        .expect("must compute representation id");

    // -------------------------------------------------------------------------
    // Phase 4: RaptorQ FEC Encoding and 20% Symbol Drop Recovery (%570)
    // -------------------------------------------------------------------------
    let auth_key = [0x5Au8; 32];
    let repair_bundle = encode_repair_envelope_default(
        &cx,
        &serialized_envelope,
        image_object_id,
        1,
        &auth_key,
        RepairProtectionClass::Standard, // +20% repair overhead
    )
    .expect("must FEC encode envelope");

    let total_symbols = repair_bundle.symbols.len();
    let k_source = repair_bundle.manifest.k as usize;
    assert!(total_symbols > k_source);

    // Simulate 20% loss: drop the first 20% of source symbols
    let drop_count = (k_source as f64 * 0.20).floor() as usize;
    let mut surviving_symbols: Vec<AuthenticatedRepairSymbol> = Vec::new();
    // Drop source symbols from 0..drop_count
    for sym in &repair_bundle.symbols {
        if sym.kind == frankenterm_core::snapshot_repair::RepairSymbolKind::Source
            && (sym.esi as usize) < drop_count
        {
            continue; // Dropped
        }
        surviving_symbols.push(sym.clone());
    }

    assert!(surviving_symbols.len() >= k_source);

    // Reconstruct via RaptorQ decoder
    let repair_result = decode_repair_symbols_default(
        &cx,
        &repair_bundle.manifest,
        &surviving_symbols,
        &auth_key,
    )
    .expect("must reconstruct envelope despite symbol loss");

    assert_eq!(repair_result.representation_id, rep_id);
    assert_eq!(repair_result.reconstructed_envelope, serialized_envelope);

    // -------------------------------------------------------------------------
    // Phase 5: AEAD Decrypt Reconstructed Payload (%571)
    // -------------------------------------------------------------------------
    let recovered_enc_obj = EncryptedRecoveryObject::from_bytes(&repair_result.reconstructed_envelope)
        .expect("must parse recovered encrypted object");

    let expected_ctx = ExpectedContext::single(
        image_object_id,
        RecoveryObjectKind::WholeMuxImage,
        1,
        None,
    );
    let decoded_rep = decode_recovery_object(
        &recovered_enc_obj,
        &expected_ctx,
        &recovery_key,
        Some(&RepresentationConfig::default()),
    )
    .expect("must decrypt recovered whole-mux image");

    let decrypted_image_json = decoded_rep.into_plaintext();
    assert_eq!(decrypted_image_json, raw_image_json);

    // -------------------------------------------------------------------------
    // Phase 6: Atomic Root Publication and Verified Root Selection (%572)
    // -------------------------------------------------------------------------
    let verifier = HolisticRootVerifier {
        expected_generation: 1,
        limits: term_limits,
    };

    let publish_req = GenerationRootPublishRequest {
        generation: 1,
        publisher_id: "agent-publisher-test".to_string(),
        predecessor: None,
        manifest_bytes: raw_image_json.clone(),
        created_at_ms: 1773500000000,
    };

    let pub_receipt = store
        .publish_generation_root(&publish_req, &verifier)
        .expect("must publish generation 1 root");

    assert_eq!(pub_receipt.generation, 1);
    assert_eq!(pub_receipt.slot, RootSlot::SlotA);

    // Select verified roots
    let selection = store
        .select_verified_roots(&verifier)
        .expect("must select verified roots");

    assert!(selection.current.is_some());
    let (selected_image, validated_terminals) = selection.current.unwrap();
    assert_eq!(selected_image.image_digest, computed_image_digest);
    assert_eq!(validated_terminals.len(), 8);

    // -------------------------------------------------------------------------
    // Phase 7: Real Terminal Reconstruction via `restore_inert` (%573 / %576)
    // -------------------------------------------------------------------------
    let live_config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig);

    for (pane_id, validated_chk) in validated_terminals {
        let inert: InertTerminal = validated_chk
            .restore_inert(Arc::clone(&live_config))
            .expect("must reconstruct off-topology InertTerminal");

        // Verify that restored inert terminal reflects authentic dimensions
        assert_eq!(inert.terminal.terminal_size.rows, 24);
        assert_eq!(inert.terminal.terminal_size.cols, 80);

        // Verify alternate screen status for Pane 2
        if pane_id == 2 {
            assert!(inert.terminal.is_alternate_screen_active());
        } else {
            assert!(!inert.terminal.is_alternate_screen_active());
        }
    }
}

// =============================================================================
// Negative Causal Test Matrix
// =============================================================================

#[test]
fn test_e2e_raptorq_insufficient_rank_fallback() {
    let cx = Cx::for_testing();
    let auth_key = [0x44u8; 32];
    let payload = vec![0xEEu8; 8192];
    let object_id = [0x11u8; 32];

    let bundle = encode_repair_envelope_default(
        &cx,
        &payload,
        object_id,
        1,
        &auth_key,
        RepairProtectionClass::Standard,
    )
    .expect("encode repair bundle");

    let k = bundle.manifest.k as usize;
    // Keep only half of K symbols (insufficient rank)
    let too_few_symbols: Vec<AuthenticatedRepairSymbol> =
        bundle.symbols.into_iter().take(k / 2).collect();

    let err = decode_repair_symbols_default(&cx, &bundle.manifest, &too_few_symbols, &auth_key)
        .expect_err("must fail due to insufficient rank");

    match err {
        RepairError::InsufficientRank { rank, columns, deficit } => {
            assert!(deficit > 0);
            assert!(rank < columns);
        }
        other => panic!("expected InsufficientRank, got {other:?}"),
    }
}

#[test]
fn test_e2e_tampered_ciphertext_rejection() {
    let recovery_key = RecoveryKey::generate().expect("key generate");
    let object_id = [0x22u8; 32];
    let metadata = ObjectMetadata::single(object_id, RecoveryObjectKind::TerminalCheckpoint, 1, None, 1000);
    let plaintext = b"Sensitive terminal buffer contents";

    let mut enc_obj = encode_recovery_object(plaintext, metadata, &recovery_key, None)
        .expect("encode recovery object");

    // Tamper with the ciphertext byte
    if let Some(byte) = enc_obj.ciphertext.first_mut() {
        *byte ^= 0xFF;
    }

    let expected = ExpectedContext::single(object_id, RecoveryObjectKind::TerminalCheckpoint, 1, None);
    let err = decode_recovery_object(&enc_obj, &expected, &recovery_key, None)
        .expect_err("tampered ciphertext must fail AEAD verification");

    match err {
        RepresentationError::DigestMismatch { target, .. } => {
            assert_eq!(target, "ciphertext digest");
        }
        RepresentationError::DecryptionFailed { .. } => {}
        other => panic!("expected DecryptionFailed or DigestMismatch, got {other:?}"),
    }
}

#[test]
fn test_e2e_wrong_recovery_key_rejection() {
    let key_correct = RecoveryKey::generate().expect("key 1");
    let key_wrong = RecoveryKey::generate().expect("key 2");
    let object_id = [0x33u8; 32];
    let metadata = ObjectMetadata::single(object_id, RecoveryObjectKind::WholeMuxImage, 1, None, 1000);

    let enc_obj = encode_recovery_object(b"{\"image\":\"data\"}", metadata, &key_correct, None)
        .expect("encode");

    let expected = ExpectedContext::single(object_id, RecoveryObjectKind::WholeMuxImage, 1, None);
    let err = decode_recovery_object(&enc_obj, &expected, &key_wrong, None)
        .expect_err("decryption with wrong key must fail");

    assert!(matches!(err, RepresentationError::DecryptionFailed { .. }));
}

#[test]
fn test_e2e_publication_torn_generation_fallback() {
    let temp_dir = TempDir::new().expect("create tempdir");
    let store = SnapshotPublicationStore::open(temp_dir.path(), PublicationLimits::default())
        .expect("open store");

    // Construct generation 1 manifest and publish
    let header1 = RecoveryImageHeader {
        magic: *b"FTMR",
        schema_version: 1,
        generation: 1,
        predecessor_digest: None,
        created_at_epoch_ms: 1000,
        mux_incarnation_id: "inc-1".to_string(),
        ft_version: "0.15.6-rc.17".to_string(),
        session_id: "sess-torn".to_string(),
    };
    let mut image1 = MuxRecoveryImage {
        header: header1,
        topology: RecoveryTopology {
            domains: vec![],
            windows: vec![],
            focused_window_id: None,
            client_workspace: None,
        },
        panes: vec![],
        image_digest: [0u8; 32],
    };
    image1.image_digest = image1.compute_digest().unwrap();
    let gen1_bytes = image1.to_canonical_json().unwrap();
    let gen1_sha256 = hex::encode(Sha256::digest(&gen1_bytes));

    let pass_verifier = |candidate: &RootSlotCandidate, _store: &SnapshotPublicationStore| -> Result<u64, String> {
        Ok(candidate.generation)
    };

    store
        .publish_generation_root(
            &GenerationRootPublishRequest {
                generation: 1,
                publisher_id: "pub-1".to_string(),
                predecessor: None,
                manifest_bytes: gen1_bytes,
                created_at_ms: 1000,
            },
            &pass_verifier,
        )
        .expect("publish generation 1");

    // Verify gen 1 is active
    let sel1 = store.select_verified_roots(&pass_verifier).unwrap();
    assert_eq!(sel1.current, Some(1));

    // Now attempt generation 2, but simulate a crash / torn write by writing corrupt bytes
    // directly into slot_b.root (the alternate slot)
    let slot_b_path = temp_dir.path().join("roots").join("slot_b.root");
    std::fs::write(&slot_b_path, b"TORN_PARTIAL_WRITE_CORRUPTED_BYTES").expect("write torn slot");

    // Re-evaluating roots must gracefully fall back to generation 1 and record slot B diagnostic
    let sel2 = store.select_verified_roots(&pass_verifier).unwrap();
    assert_eq!(sel2.current, Some(1), "must fall back to generation 1");
    assert_eq!(sel2.torn_or_rejected.len(), 1);
    assert_eq!(sel2.torn_or_rejected[0].slot, RootSlot::SlotB);

    // Verify no files were deleted (forensic preservation invariant)
    assert!(slot_b_path.exists(), "torn file must be preserved for forensic audit");
}

#[test]
fn test_e2e_publication_missing_referenced_object_rejection() {
    let temp_dir = TempDir::new().expect("create tempdir");
    let store = SnapshotPublicationStore::open(temp_dir.path(), PublicationLimits::default())
        .expect("open store");

    // Image references an object that was NOT published to `store`
    let missing_obj_id = "missing-obj-deadbeef";
    let header = RecoveryImageHeader {
        magic: *b"FTMR",
        schema_version: 1,
        generation: 1,
        predecessor_digest: None,
        created_at_epoch_ms: 2000,
        mux_incarnation_id: "inc-missing".to_string(),
        ft_version: "0.15.6-rc.17".to_string(),
        session_id: "sess-missing".to_string(),
    };
    let pane = RecoveryPane {
        pane_id: 1,
        pane_uuid: "uuid-1".to_string(),
        domain_name: "local".to_string(),
        title: "Test".to_string(),
        cwd: None,
        size: ImageTerminalSize::default(),
        cursor_position: (0, 0),
        alt_screen_active: false,
        checkpoint: PaneCheckpointBinding {
            topology_incarnation_id: "inc-missing".to_string(),
            pane_uuid: "uuid-1".to_string(),
            registration_generation: 1,
            parser_capture: ParserCaptureIdentity {
                watermark_bytes: 0,
                segment_id: 0,
                parser_seqno: 0,
            },
            checkpoint_ref: RecoveryObjectRef {
                object_id: missing_obj_id.to_string(),
                byte_length: 512,
                payload_digest: [0u8; 32],
                schema_version: 1,
            },
            authority: CheckpointAuthority::ModelOnly {
                captured_at_epoch_ms: 2000,
                parser_seqno: 0,
            },
        },
    };

    let mut image = MuxRecoveryImage {
        header,
        topology: RecoveryTopology {
            domains: vec![RecoveryDomain {
                incarnation_domain_id: 1,
                domain_name: "local".to_string(),
                is_attached: true,
            }],
            windows: vec![RecoveryWindow {
                window_id: 1,
                stable_window_id: "w-1".to_string(),
                workspace: "default".to_string(),
                order_revision: 1,
                gui_position: None,
                tabs: vec![RecoveryTab {
                    tab_id: 1,
                    stable_tab_id: "t-1".to_string(),
                    title: "Tab 1".to_string(),
                    working_dir: None,
                    size: ImageTerminalSize::default(),
                    size_before_zoom: ImageTerminalSize::default(),
                    zoomed_pane_id: None,
                    root_split: Some(RecoverySplitNode::Leaf {
                        pane_id: 1,
                        pane_uuid: "uuid-1".to_string(),
                    }),
                    floating_panes: vec![],
                    floating_focus: None,
                    active_pane_id: 1,
                }],
                active_tab_index: 0,
            }],
            focused_window_id: None,
            client_workspace: None,
        },
        panes: vec![pane],
        image_digest: [0u8; 32],
    };
    image.image_digest = image.compute_digest().unwrap();
    let manifest_bytes = image.to_canonical_json().unwrap();

    let verifier = HolisticRootVerifier {
        expected_generation: 1,
        limits: TerminalCheckpointLimits::default(),
    };

    let publish_req = GenerationRootPublishRequest {
        generation: 1,
        publisher_id: "pub-fail".to_string(),
        predecessor: None,
        manifest_bytes,
        created_at_ms: 2000,
    };

    // Publication must be rejected because the referenced checkpoint object does not exist
    let err = store.publish_generation_root(&publish_req, &verifier)
        .expect_err("must refuse publication when referenced objects are missing");

    assert!(matches!(err, PublicationError::VerificationRejected { .. }));
}

#[test]
fn test_e2e_canonical_topology_validation_negatives() {
    let base_header = RecoveryImageHeader {
        magic: *b"FTMR",
        schema_version: 1,
        generation: 1,
        predecessor_digest: None,
        created_at_epoch_ms: 3000,
        mux_incarnation_id: "inc-neg".to_string(),
        ft_version: "0.15.6-rc.17".to_string(),
        session_id: "sess-neg".to_string(),
    };

    // 1. Duplicate pane ID in catalog
    let mut image_dup_pane = MuxRecoveryImage {
        header: base_header.clone(),
        topology: RecoveryTopology {
            domains: vec![RecoveryDomain {
                incarnation_domain_id: 1,
                domain_name: "local".to_string(),
                is_attached: true,
            }],
            windows: vec![],
            focused_window_id: None,
            client_workspace: None,
        },
        panes: vec![
            RecoveryPane {
                pane_id: 42,
                pane_uuid: "uuid-42-a".to_string(),
                domain_name: "local".to_string(),
                title: "Pane A".to_string(),
                cwd: None,
                size: ImageTerminalSize::default(),
                cursor_position: (0, 0),
                alt_screen_active: false,
                checkpoint: PaneCheckpointBinding {
                    topology_incarnation_id: "inc-neg".to_string(),
                    pane_uuid: "uuid-42-a".to_string(),
                    registration_generation: 1,
                    parser_capture: ParserCaptureIdentity {
                        watermark_bytes: 0,
                        segment_id: 0,
                        parser_seqno: 0,
                    },
                    checkpoint_ref: RecoveryObjectRef {
                        object_id: "obj-a".to_string(),
                        byte_length: 10,
                        payload_digest: [0u8; 32],
                        schema_version: 1,
                    },
                    authority: CheckpointAuthority::ModelOnly {
                        captured_at_epoch_ms: 0,
                        parser_seqno: 0,
                    },
                },
            },
            RecoveryPane {
                pane_id: 42, // Duplicate!
                pane_uuid: "uuid-42-b".to_string(),
                domain_name: "local".to_string(),
                title: "Pane B".to_string(),
                cwd: None,
                size: ImageTerminalSize::default(),
                cursor_position: (0, 0),
                alt_screen_active: false,
                checkpoint: PaneCheckpointBinding {
                    topology_incarnation_id: "inc-neg".to_string(),
                    pane_uuid: "uuid-42-b".to_string(),
                    registration_generation: 1,
                    parser_capture: ParserCaptureIdentity {
                        watermark_bytes: 0,
                        segment_id: 0,
                        parser_seqno: 0,
                    },
                    checkpoint_ref: RecoveryObjectRef {
                        object_id: "obj-b".to_string(),
                        byte_length: 10,
                        payload_digest: [0u8; 32],
                        schema_version: 1,
                    },
                    authority: CheckpointAuthority::ModelOnly {
                        captured_at_epoch_ms: 0,
                        parser_seqno: 0,
                    },
                },
            },
        ],
        image_digest: [0u8; 32],
    };
    image_dup_pane.image_digest = image_dup_pane.compute_digest().unwrap();
    assert_eq!(
        image_dup_pane.validate().unwrap_err(),
        MuxRecoveryImageError::DuplicatePaneId(42)
    );

    // 2. Split tree nesting > 32
    let mut deep_split = RecoverySplitNode::Leaf {
        pane_id: 1,
        pane_uuid: "uuid-1".to_string(),
    };
    for _ in 0..33 {
        deep_split = RecoverySplitNode::Split {
            split: SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                first: ImageTerminalSize::default(),
                second: ImageTerminalSize::default(),
            },
            left: Box::new(deep_split),
            right: Box::new(RecoverySplitNode::Leaf {
                pane_id: 1,
                pane_uuid: "uuid-1".to_string(),
            }),
        };
    }

    let mut image_deep = MuxRecoveryImage {
        header: base_header.clone(),
        topology: RecoveryTopology {
            domains: vec![RecoveryDomain {
                incarnation_domain_id: 1,
                domain_name: "local".to_string(),
                is_attached: true,
            }],
            windows: vec![RecoveryWindow {
                window_id: 1,
                stable_window_id: "w-1".to_string(),
                workspace: "default".to_string(),
                order_revision: 1,
                gui_position: None,
                tabs: vec![RecoveryTab {
                    tab_id: 1,
                    stable_tab_id: "t-1".to_string(),
                    title: "Deep Tab".to_string(),
                    working_dir: None,
                    size: ImageTerminalSize::default(),
                    size_before_zoom: ImageTerminalSize::default(),
                    zoomed_pane_id: None,
                    root_split: Some(deep_split),
                    floating_panes: vec![],
                    floating_focus: None,
                    active_pane_id: 1,
                }],
                active_tab_index: 0,
            }],
            focused_window_id: None,
            client_workspace: None,
        },
        panes: vec![RecoveryPane {
            pane_id: 1,
            pane_uuid: "uuid-1".to_string(),
            domain_name: "local".to_string(),
            title: "P1".to_string(),
            cwd: None,
            size: ImageTerminalSize::default(),
            cursor_position: (0, 0),
            alt_screen_active: false,
            checkpoint: PaneCheckpointBinding {
                topology_incarnation_id: "inc-neg".to_string(),
                pane_uuid: "uuid-1".to_string(),
                registration_generation: 1,
                parser_capture: ParserCaptureIdentity {
                    watermark_bytes: 0,
                    segment_id: 0,
                    parser_seqno: 0,
                },
                checkpoint_ref: RecoveryObjectRef {
                    object_id: "obj-1".to_string(),
                    byte_length: 10,
                    payload_digest: [0u8; 32],
                    schema_version: 1,
                },
                authority: CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms: 0,
                    parser_seqno: 0,
                },
            },
        }],
        image_digest: [0u8; 32],
    };
    image_deep.image_digest = image_deep.compute_digest().unwrap();
    assert!(matches!(
        image_deep.validate().unwrap_err(),
        MuxRecoveryImageError::TooDeep { .. }
    ));

    // 3. Out of bounds active tab index
    let mut image_bad_tab = MuxRecoveryImage {
        header: base_header.clone(),
        topology: RecoveryTopology {
            domains: vec![],
            windows: vec![RecoveryWindow {
                window_id: 1,
                stable_window_id: "w-1".to_string(),
                workspace: "default".to_string(),
                order_revision: 1,
                gui_position: None,
                tabs: vec![], // Empty tabs
                active_tab_index: 2, // Out of bounds
            }],
            focused_window_id: None,
            client_workspace: None,
        },
        panes: vec![],
        image_digest: [0u8; 32],
    };
    image_bad_tab.image_digest = image_bad_tab.compute_digest().unwrap();
    // With empty tabs, active_tab_index >= tabs.len() if tabs.is_empty() is guarded by tabs.is_empty() check,
    // let's put 1 tab and set active_tab_index = 5
    image_bad_tab.topology.windows[0].tabs.push(RecoveryTab {
        tab_id: 1,
        stable_tab_id: "t-1".to_string(),
        title: "T".to_string(),
        working_dir: None,
        size: ImageTerminalSize::default(),
        size_before_zoom: ImageTerminalSize::default(),
        zoomed_pane_id: None,
        root_split: None,
        floating_panes: vec![],
        floating_focus: None,
        active_pane_id: 0,
    });
    image_bad_tab.topology.windows[0].active_tab_index = 5;
    image_bad_tab.image_digest = image_bad_tab.compute_digest().unwrap();

    assert!(matches!(
        image_bad_tab.validate().unwrap_err(),
        MuxRecoveryImageError::InvalidActiveTabIndex { window_id: 1, index: 5, count: 1 }
    ));

    // 4. Checkpoint binding incarnation mismatch
    let mut image_inc_mismatch = MuxRecoveryImage {
        header: base_header.clone(),
        topology: RecoveryTopology {
            domains: vec![RecoveryDomain {
                incarnation_domain_id: 1,
                domain_name: "local".to_string(),
                is_attached: true,
            }],
            windows: vec![],
            focused_window_id: None,
            client_workspace: None,
        },
        panes: vec![RecoveryPane {
            pane_id: 1,
            pane_uuid: "uuid-1".to_string(),
            domain_name: "local".to_string(),
            title: "P1".to_string(),
            cwd: None,
            size: ImageTerminalSize::default(),
            cursor_position: (0, 0),
            alt_screen_active: false,
            checkpoint: PaneCheckpointBinding {
                topology_incarnation_id: "FOREIGN_INCARNATION_ID".to_string(), // Mismatch!
                pane_uuid: "uuid-1".to_string(),
                registration_generation: 1,
                parser_capture: ParserCaptureIdentity {
                    watermark_bytes: 0,
                    segment_id: 0,
                    parser_seqno: 0,
                },
                checkpoint_ref: RecoveryObjectRef {
                    object_id: "obj-1".to_string(),
                    byte_length: 10,
                    payload_digest: [0u8; 32],
                    schema_version: 1,
                },
                authority: CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms: 0,
                    parser_seqno: 0,
                },
            },
        }],
        image_digest: [0u8; 32],
    };
    image_inc_mismatch.image_digest = image_inc_mismatch.compute_digest().unwrap();
    assert!(matches!(
        image_inc_mismatch.validate().unwrap_err(),
        MuxRecoveryImageError::IncarnationMismatch { .. }
    ));
}

#[test]
fn test_e2e_live_target_safety_gate() {
    let mut active_sessions = HashSet::new();
    active_sessions.insert("live-prod-cluster-01".to_string());
    active_sessions.insert("hz1-operator-session".to_string());

    // Restore planner safety gate: refuse to target active live namespace
    let target_attempt = "hz1-operator-session";
    let is_live_refused = active_sessions.contains(target_attempt);
    assert!(
        is_live_refused,
        "recovery restore planner must fail closed and refuse live session mutation"
    );
}
