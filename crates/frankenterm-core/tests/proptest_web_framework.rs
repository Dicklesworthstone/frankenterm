//! Property-based tests for public `web_framework` response helpers.

use asupersync::stream;
use frankenterm_core::web_framework::{
    FrameworkResponseBody, FrameworkStatusCode, json_response_with_status, sse_stream_response,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TestPayload {
    message: String,
    count: u32,
    ok: bool,
}

fn arb_payload() -> impl Strategy<Value = TestPayload> {
    ("[A-Za-z0-9 _.-]{0,40}", any::<u32>(), any::<bool>())
        .prop_map(|(message, count, ok)| TestPayload { message, count, ok })
}

fn header_value<'a>(headers: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_slice())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn json_response_with_status_preserves_status_and_payload(
        payload in arb_payload(),
        status in prop_oneof![
            Just(FrameworkStatusCode::OK),
            Just(FrameworkStatusCode::CREATED),
            Just(FrameworkStatusCode::BAD_REQUEST),
            Just(FrameworkStatusCode::INTERNAL_SERVER_ERROR),
        ],
    ) {
        let response = json_response_with_status(status, &payload);
        let headers = response.headers();

        prop_assert_eq!(response.status(), status);
        prop_assert_eq!(header_value(headers, "content-type"), Some(&b"application/json"[..]));

        match response.body_ref() {
            FrameworkResponseBody::Bytes(bytes) => {
                let decoded: TestPayload = serde_json::from_slice(bytes).unwrap();
                prop_assert_eq!(decoded, payload);
            }
            other => prop_assert!(false, "expected bytes body, got {:?}", other),
        }
    }

    #[test]
    fn json_response_with_status_emits_non_empty_json_body(payload in arb_payload()) {
        let response = json_response_with_status(FrameworkStatusCode::OK, &payload);

        match response.body_ref() {
            FrameworkResponseBody::Bytes(bytes) => {
                prop_assert!(!bytes.is_empty());
                prop_assert_eq!(response.body_ref().len(), bytes.len());
                prop_assert!(!response.body_ref().is_empty());
            }
            other => prop_assert!(false, "expected bytes body, got {:?}", other),
        }
    }

    #[test]
    fn sse_stream_response_sets_standard_headers(chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..32), 0..4)) {
        let response = sse_stream_response(stream::iter(chunks));
        let headers = response.headers();

        prop_assert_eq!(response.status(), FrameworkStatusCode::OK);
        prop_assert_eq!(header_value(headers, "content-type"), Some(&b"text/event-stream"[..]));
        prop_assert_eq!(header_value(headers, "cache-control"), Some(&b"no-cache"[..]));
        prop_assert_eq!(header_value(headers, "connection"), Some(&b"keep-alive"[..]));
        prop_assert_eq!(header_value(headers, "x-accel-buffering"), Some(&b"no"[..]));

        match response.body_ref() {
            FrameworkResponseBody::Stream(_) => {}
            other => prop_assert!(false, "expected stream body, got {:?}", other),
        }
    }
}
