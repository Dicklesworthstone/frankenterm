#![cfg(feature = "serde")]

use frankenterm_blob_leases::ContentId;
use proptest::prelude::*;

fn arb_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..256)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn content_id_json_roundtrip(bytes in arb_bytes()) {
        let value = ContentId::for_bytes(&bytes);
        let json = serde_json::to_string(&value).unwrap();
        let back: ContentId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn content_id_hash_bytes_survive_json_roundtrip(bytes in arb_bytes()) {
        let value = ContentId::for_bytes(&bytes);
        let before = value.as_hash_bytes();
        let json = serde_json::to_string(&value).unwrap();
        let back: ContentId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.as_hash_bytes(), before);
    }
}
