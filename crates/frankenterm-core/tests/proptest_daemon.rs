#![cfg(feature = "semantic-search")]

//! Property-based tests for search daemon protocol, worker, and client.
//!
//! Bead: ft-3i6bf
//!
//! Validates:
//! 1. Protocol round-trip: DaemonRequest serializes/deserializes identically
//! 2. Protocol round-trip: DaemonResponse serializes/deserializes identically
//! 3. EmbedRequest field preservation through serialization
//! 4. EmbedResponse field/vector preservation through serialization
//! 5. Size limit enforcement on encode
//! 6. Size limit enforcement on decode
//! 7. Ping/Shutdown tagged JSON structure stability
//! 8. Cross-type rejection: request bytes don't decode as wrong variant
//! 9. Worker: model parsing accepts valid models
//! 10. Worker: model parsing rejects invalid models
//! 11. Worker: request ID passes through to response
//! 12. Worker: same text produces same embedding (determinism)
//! 13. Worker: different texts produce different embeddings (sensitivity)
//! 14. Worker: dimension override controls vector length
//! 15. Worker: processed counter increments only on success
//! 16. Client: transport errors propagate correctly
//! 17. Client: timeout enforcement rejects slow transports
//! 18. Client: ping succeeds with loopback transport
//! 19. Client: embed succeeds with loopback transport
//! 20. Client: embed returns correct dimensions via loopback
//! 21. Protocol: Pong response round-trips through JSON
//! 22. Protocol: Error response preserves message through round-trip
//! 23. Worker: empty text always produces valid embeddings
//! 24. Worker: unicode text always produces valid embeddings
//! 25. Protocol: encoded request bytes are valid UTF-8 JSON
//! 26. Protocol: encoded response bytes are valid UTF-8 JSON
//! 27. Client: embed error response from daemon maps to EmbedClientError::Daemon
//! 28. Server: handle_encoded returns valid JSON for any valid request
//! 29. Server: shutdown sets running to false
//! 30. Server: embed after shutdown returns error

use proptest::prelude::*;

use frankenterm_core::search::daemon::{
    DaemonRequest, DaemonResponse, EmbedClient, EmbedRequest, EmbedResponse, EmbedServer,
    EmbedWorker,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_embed_id() -> impl Strategy<Value = u64> {
    any::<u64>()
}

fn arb_text() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9 _\\-]{0,200}")
        .unwrap()
        .boxed()
}

fn arb_text_nonempty() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9 _\\-]{1,200}")
        .unwrap()
        .boxed()
}

fn arb_valid_model() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some(String::new())),
        Just(Some("hash".to_string())),
        Just(Some("fnv1a-hash".to_string())),
        (1_usize..512).prop_map(|d| Some(format!("fnv1a-hash-{d}"))),
    ]
}

fn arb_invalid_model() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("unknown-model".to_string()),
        Just("openai-ada-002".to_string()),
        Just("fastembed-e5-large".to_string()),
        Just("fnv1a-hash-0".to_string()),
        Just("fnv1a-hash-abc".to_string()),
        proptest::string::string_regex("[a-z]{3,20}")
            .unwrap()
            .prop_filter("must not match valid models", |s| {
                s != "hash" && !s.starts_with("fnv1a-hash")
            }),
    ]
}

fn arb_embed_request() -> impl Strategy<Value = EmbedRequest> {
    (arb_embed_id(), arb_text(), arb_valid_model()).prop_map(|(id, text, model)| EmbedRequest {
        id,
        text,
        model,
    })
}

fn arb_vector(dim: usize) -> impl Strategy<Value = Vec<f32>> {
    proptest::collection::vec(
        any::<f32>().prop_filter("must be finite", |f| f.is_finite()),
        dim,
    )
}

fn arb_embed_response() -> impl Strategy<Value = EmbedResponse> {
    (
        arb_embed_id(),
        arb_vector(32),
        arb_text_nonempty(),
        0_u64..10_000,
    )
        .prop_map(|(id, vector, model, elapsed_ms)| EmbedResponse {
            id,
            vector,
            model,
            elapsed_ms,
        })
}

fn arb_daemon_request() -> impl Strategy<Value = DaemonRequest> {
    prop_oneof![
        Just(DaemonRequest::Ping),
        Just(DaemonRequest::Shutdown),
        arb_embed_request().prop_map(DaemonRequest::Embed),
    ]
}

fn arb_daemon_response() -> impl Strategy<Value = DaemonResponse> {
    prop_oneof![
        Just(DaemonResponse::Pong),
        arb_text().prop_map(DaemonResponse::Error),
        arb_embed_response().prop_map(DaemonResponse::Embed),
    ]
}

fn loopback_transport(server: &EmbedServer) -> impl FnMut(&[u8]) -> Result<Vec<u8>, String> + '_ {
    move |request_bytes| Ok(server.handle_encoded(request_bytes))
}

// =============================================================================
// Protocol round-trip properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // 1. DaemonRequest round-trip serialization
    #[test]
    fn request_round_trip(request in arb_daemon_request()) {
        let bytes = request.to_json_bytes().expect("encode request");
        let decoded = DaemonRequest::from_json_bytes(&bytes).expect("decode request");
        prop_assert_eq!(decoded, request);
    }

    // 2. DaemonResponse round-trip serialization
    #[test]
    fn response_round_trip(response in arb_daemon_response()) {
        let bytes = response.to_json_bytes().expect("encode response");
        let decoded = DaemonResponse::from_json_bytes(&bytes).expect("decode response");
        prop_assert_eq!(decoded, response);
    }

    // 3. EmbedRequest field preservation
    #[test]
    fn embed_request_fields_preserved(req in arb_embed_request()) {
        let daemon_req = DaemonRequest::Embed(req.clone());
        let bytes = daemon_req.to_json_bytes().expect("encode");
        let decoded = DaemonRequest::from_json_bytes(&bytes).expect("decode");
        match decoded {
            DaemonRequest::Embed(decoded_req) => {
                prop_assert_eq!(decoded_req.id, req.id);
                prop_assert_eq!(decoded_req.text, req.text);
                prop_assert_eq!(decoded_req.model, req.model);
            }
            other => prop_assert!(false, "expected Embed, got {:?}", other),
        }
    }

    // 4. EmbedResponse vector preservation
    #[test]
    fn embed_response_vector_preserved(resp in arb_embed_response()) {
        let daemon_resp = DaemonResponse::Embed(resp.clone());
        let bytes = daemon_resp.to_json_bytes().expect("encode");
        let decoded = DaemonResponse::from_json_bytes(&bytes).expect("decode");
        match decoded {
            DaemonResponse::Embed(decoded_resp) => {
                prop_assert_eq!(decoded_resp.id, resp.id);
                prop_assert_eq!(decoded_resp.vector.len(), resp.vector.len());
                prop_assert_eq!(decoded_resp.model, resp.model);
                prop_assert_eq!(decoded_resp.elapsed_ms, resp.elapsed_ms);
            }
            other => prop_assert!(false, "expected Embed, got {:?}", other),
        }
    }

    // 5. Size limit enforcement on encode — large text triggers PayloadTooLarge
    #[test]
    fn encode_rejects_oversized_text(extra_bytes in 1_usize..1024) {
        let text = "x".repeat(4 * 1024 * 1024 + extra_bytes);
        let request = DaemonRequest::Embed(EmbedRequest {
            id: 0,
            text,
            model: None,
        });
        let result = request.to_json_bytes();
        prop_assert!(result.is_err(), "oversized payload should be rejected");
    }

    // 6. Size limit enforcement on decode — oversized bytes rejected
    #[test]
    fn decode_rejects_oversized_bytes(extra_bytes in 1_usize..512) {
        let oversized = vec![b'x'; 4 * 1024 * 1024 + extra_bytes];
        let result = DaemonRequest::from_json_bytes(&oversized);
        prop_assert!(result.is_err(), "oversized payload should be rejected");
    }

    // 7. Ping/Shutdown have stable JSON tag
    #[test]
    fn ping_json_is_stable(_seed in 0_u32..100) {
        let bytes = DaemonRequest::Ping.to_json_bytes().expect("encode");
        let json = String::from_utf8(bytes).expect("utf8");
        prop_assert_eq!(json, r#"{"type":"ping"}"#);
    }

    // 8. Shutdown JSON is stable
    #[test]
    fn shutdown_json_is_stable(_seed in 0_u32..100) {
        let bytes = DaemonRequest::Shutdown.to_json_bytes().expect("encode");
        let json = String::from_utf8(bytes).expect("utf8");
        prop_assert_eq!(json, r#"{"type":"shutdown"}"#);
    }

    // 21. Pong response round-trips
    #[test]
    fn pong_response_round_trip(_seed in 0_u32..100) {
        let bytes = DaemonResponse::Pong.to_json_bytes().expect("encode");
        let decoded = DaemonResponse::from_json_bytes(&bytes).expect("decode");
        let is_pong = matches!(decoded, DaemonResponse::Pong);
        prop_assert!(is_pong, "expected Pong");
    }

    // 22. Error response preserves message
    #[test]
    fn error_response_preserves_message(msg in arb_text()) {
        let response = DaemonResponse::Error(msg.clone());
        let bytes = response.to_json_bytes().expect("encode");
        let decoded = DaemonResponse::from_json_bytes(&bytes).expect("decode");
        match decoded {
            DaemonResponse::Error(decoded_msg) => prop_assert_eq!(decoded_msg, msg),
            other => prop_assert!(false, "expected Error, got {:?}", other),
        }
    }

    // 25. Encoded request bytes are valid UTF-8 JSON
    #[test]
    fn request_bytes_are_valid_json(request in arb_daemon_request()) {
        let bytes = request.to_json_bytes().expect("encode");
        let json_str = String::from_utf8(bytes.clone()).expect("should be valid UTF-8");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("should be valid JSON");
        prop_assert!(parsed.is_object(), "top-level should be object");
        prop_assert!(parsed.get("type").is_some(), "should have 'type' field");
    }

    // 26. Encoded response bytes are valid UTF-8 JSON
    #[test]
    fn response_bytes_are_valid_json(response in arb_daemon_response()) {
        let bytes = response.to_json_bytes().expect("encode");
        let json_str = String::from_utf8(bytes.clone()).expect("should be valid UTF-8");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("should be valid JSON");
        prop_assert!(parsed.is_object(), "top-level should be object");
        prop_assert!(parsed.get("type").is_some(), "should have 'type' field");
    }
}

// =============================================================================
// Worker properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 9. Worker accepts valid models
    #[test]
    fn worker_accepts_valid_models(model in arb_valid_model()) {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id: 1,
            text: "test".to_string(),
            model,
        };
        let result = worker.process(&req);
        prop_assert!(result.is_ok(), "valid model should succeed");
    }

    // 10. Worker rejects invalid models
    #[test]
    fn worker_rejects_invalid_models(model in arb_invalid_model()) {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id: 1,
            text: "test".to_string(),
            model: Some(model),
        };
        let result = worker.process(&req);
        prop_assert!(result.is_err(), "invalid model should fail");
    }

    // 11. Worker request ID passes through
    #[test]
    fn worker_id_passthrough(id in arb_embed_id()) {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id,
            text: "passthrough test".to_string(),
            model: None,
        };
        let resp = worker.process(&req).expect("process succeeds");
        prop_assert_eq!(resp.id, id);
    }

    // 12. Worker determinism — same text, same embedding
    #[test]
    fn worker_deterministic(text in arb_text()) {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id: 1,
            text: text.clone(),
            model: None,
        };
        let r1 = worker.process(&req).expect("first process");
        let r2 = worker.process(&req).expect("second process");
        prop_assert_eq!(r1.vector, r2.vector, "same text should produce same embedding");
    }

    // 13. Worker sensitivity — different texts produce different embeddings
    #[test]
    fn worker_sensitive(
        text_a in arb_text_nonempty(),
        text_b in arb_text_nonempty(),
    ) {
        prop_assume!(text_a != text_b);
        let worker = EmbedWorker::new(0);
        let r1 = worker.process(&EmbedRequest {
            id: 1,
            text: text_a,
            model: None,
        }).expect("process a");
        let r2 = worker.process(&EmbedRequest {
            id: 2,
            text: text_b,
            model: None,
        }).expect("process b");
        prop_assert_ne!(r1.vector, r2.vector, "different texts should produce different embeddings");
    }

    // 14. Worker dimension override controls vector length
    #[test]
    fn worker_dimension_override(dim in 1_usize..256) {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id: 1,
            text: "dimension test".to_string(),
            model: Some(format!("fnv1a-hash-{dim}")),
        };
        let resp = worker.process(&req).expect("process succeeds");
        prop_assert_eq!(resp.vector.len(), dim, "vector length should match requested dimension");
    }

    // 15. Worker processed counter increments only on success
    #[test]
    fn worker_counter_increment(
        n_success in 0_u64..10,
        n_fail in 0_u64..10,
    ) {
        let worker = EmbedWorker::new(0);
        for i in 0..n_success {
            worker.process(&EmbedRequest {
                id: i,
                text: format!("success-{i}"),
                model: None,
            }).expect("should succeed");
        }
        for i in 0..n_fail {
            let _ = worker.process(&EmbedRequest {
                id: i,
                text: "fail".to_string(),
                model: Some("invalid-model".to_string()),
            });
        }
        prop_assert_eq!(worker.processed(), n_success, "counter should only count successes");
    }

    // 23. Worker handles empty text
    #[test]
    fn worker_empty_text(dim in 1_usize..128) {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id: 1,
            text: String::new(),
            model: Some(format!("fnv1a-hash-{dim}")),
        };
        let resp = worker.process(&req).expect("empty text should succeed");
        prop_assert_eq!(resp.vector.len(), dim);
    }

    // 24. Worker handles unicode text
    #[test]
    fn worker_unicode_text(text in "[\u{0080}-\u{FFFF}]{1,50}") {
        let worker = EmbedWorker::new(0);
        let req = EmbedRequest {
            id: 1,
            text,
            model: None,
        };
        let resp = worker.process(&req).expect("unicode text should succeed");
        prop_assert_eq!(resp.vector.len(), 128);
    }
}

// =============================================================================
// Client properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 16. Client transport errors propagate
    #[test]
    fn client_transport_error_propagates(err_msg in arb_text_nonempty()) {
        let client = EmbedClient::new("test://endpoint");
        let result = client.call_with(
            DaemonRequest::Ping,
            |_| Err(err_msg.clone()),
        );
        let is_transport_err = matches!(result, Err(ref e) if format!("{e}").contains("transport"));
        prop_assert!(is_transport_err, "transport errors should propagate: {:?}", result);
    }

    // 17. Client timeout enforcement rejects slow transports
    #[test]
    fn client_timeout_enforcement(_seed in 0_u32..20) {
        let client = EmbedClient::new("test://endpoint").with_timeout_ms(1);
        let result = client.call_with(
            DaemonRequest::Ping,
            |_| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                DaemonResponse::Pong
                    .to_json_bytes()
                    .map_err(|e| e.to_string())
            },
        );
        let is_timeout = matches!(result, Err(ref e) if format!("{e}").contains("timed out"));
        prop_assert!(is_timeout, "slow transport should trigger timeout: {:?}", result);
    }

    // 18. Client ping succeeds via loopback
    #[test]
    fn client_ping_loopback(_seed in 0_u32..50) {
        let client = EmbedClient::new("loopback://test");
        let server = EmbedServer::new(0);
        let result = client.ping_with(loopback_transport(&server));
        prop_assert!(result.is_ok(), "ping via loopback should succeed");
    }

    // 19. Client embed succeeds via loopback
    #[test]
    fn client_embed_loopback(
        id in arb_embed_id(),
        text in arb_text(),
    ) {
        let client = EmbedClient::new("loopback://test");
        let server = EmbedServer::new(0);
        let result = client.embed_with(id, text, None, loopback_transport(&server));
        prop_assert!(result.is_ok(), "embed via loopback should succeed");
        let resp = result.unwrap();
        prop_assert_eq!(resp.id, id, "ID should pass through");
    }

    // 20. Client embed returns correct dimensions via loopback
    #[test]
    fn client_embed_dimensions(dim in 1_usize..256) {
        let client = EmbedClient::new("loopback://test");
        let server = EmbedServer::new(0);
        let result = client.embed_with(
            1,
            "dimension test",
            Some(format!("fnv1a-hash-{dim}")),
            loopback_transport(&server),
        );
        prop_assert!(result.is_ok(), "embed should succeed");
        let resp = result.unwrap();
        prop_assert_eq!(resp.vector.len(), dim, "vector dimension should match");
    }

    // 27. Client maps daemon error response correctly
    #[test]
    fn client_maps_daemon_error(_seed in 0_u32..50) {
        let client = EmbedClient::new("loopback://test");
        let server = EmbedServer::new(0);
        let result = client.embed_with(
            1,
            "test",
            Some("invalid-model".to_string()),
            loopback_transport(&server),
        );
        let is_daemon_err = matches!(result, Err(ref e) if format!("{e}").contains("daemon"));
        prop_assert!(is_daemon_err, "invalid model should produce daemon error: {:?}", result);
    }
}

// =============================================================================
// Server properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 28. Server handle_encoded returns valid JSON for any valid request
    #[test]
    fn server_handle_encoded_returns_valid_json(request in arb_daemon_request()) {
        let server = EmbedServer::new(0);
        let request_bytes = request.to_json_bytes().expect("encode request");
        let response_bytes = server.handle_encoded(&request_bytes);
        let json_str = String::from_utf8(response_bytes).expect("response should be UTF-8");
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json_str);
        prop_assert!(parsed.is_ok(), "response should be valid JSON");
    }

    // 29. Server shutdown sets running to false
    #[test]
    fn server_shutdown_sets_running_false(_seed in 0_u32..50) {
        let server = EmbedServer::new(0);
        prop_assert!(server.is_running(), "server should start running");
        server.handle(DaemonRequest::Shutdown);
        prop_assert!(!server.is_running(), "server should stop after shutdown");
    }

    // 30. Server embed after shutdown returns error
    #[test]
    fn server_embed_after_shutdown(text in arb_text()) {
        let server = EmbedServer::new(0);
        server.handle(DaemonRequest::Shutdown);
        let response = server.handle(DaemonRequest::Embed(EmbedRequest {
            id: 1,
            text,
            model: None,
        }));
        let is_err = matches!(response, DaemonResponse::Error(_));
        prop_assert!(is_err, "embed after shutdown should be error");
    }
}
