//! Property-based tests for `rope` module.
//!
//! Verifies correctness invariants of the Rope data structure using proptest:
//! - Content preservation (rope == reference string)
//! - Append/prepend correctness
//! - Insert correctness at arbitrary positions
//! - Delete correctness at arbitrary ranges
//! - Split/concat roundtrip
//! - Substring correctness
//! - Char-at consistency
//! - Serde roundtrip
//! - Line operations

use frankenterm_core::rope::Rope;
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 \\n]{0,200}"
}

fn short_text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9]{0,50}"
}

fn large_text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 \\n]{500,2000}"
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Content preservation ───────────────────────────────────────

    #[test]
    fn from_str_preserves_content(text in text_strategy()) {
        let rope = Rope::from_str(&text);
        prop_assert_eq!(rope.to_string_full(), text);
    }

    #[test]
    fn from_str_preserves_len(text in text_strategy()) {
        let rope = Rope::from_str(&text);
        prop_assert_eq!(rope.len(), text.len());
    }

    #[test]
    fn large_text_preserved(text in large_text_strategy()) {
        let expected_len = text.len();
        let rope = Rope::from_str(&text);
        prop_assert_eq!(rope.to_string_full(), text);
        prop_assert_eq!(rope.len(), expected_len);
    }

    // ── Append correctness ─────────────────────────────────────────

    #[test]
    fn append_matches_string_concat(
        base in text_strategy(),
        suffix in short_text_strategy()
    ) {
        let mut rope = Rope::from_str(&base);
        rope.append(&suffix);
        let expected = format!("{}{}", base, suffix);
        prop_assert_eq!(rope.to_string_full(), expected);
    }

    #[test]
    fn multiple_appends(parts in prop::collection::vec(short_text_strategy(), 1..10)) {
        let mut rope = Rope::new();
        let mut reference = String::new();
        for part in &parts {
            rope.append(part);
            reference.push_str(part);
        }
        prop_assert_eq!(rope.to_string_full(), reference);
    }

    // ── Prepend correctness ────────────────────────────────────────

    #[test]
    fn prepend_matches_string_concat(
        base in text_strategy(),
        prefix in short_text_strategy()
    ) {
        let mut rope = Rope::from_str(&base);
        rope.prepend(&prefix);
        let expected = format!("{}{}", prefix, base);
        prop_assert_eq!(rope.to_string_full(), expected);
    }

    // ── Insert correctness ─────────────────────────────────────────

    #[test]
    fn insert_at_position(
        base in "[a-zA-Z]{1,100}",
        insert_text in "[0-9]{1,20}",
        pos_frac in 0.0..1.0f64
    ) {
        let pos = (pos_frac * base.len() as f64) as usize;
        let pos = pos.min(base.len());

        let mut rope = Rope::from_str(&base);
        rope.insert(pos, &insert_text);

        let mut expected = base.clone();
        expected.insert_str(pos, &insert_text);

        prop_assert_eq!(rope.to_string_full(), expected);
    }

    // ── Delete correctness ─────────────────────────────────────────

    #[test]
    fn delete_range(
        text in "[a-zA-Z]{5,100}",
        start_frac in 0.0..1.0f64,
        len_frac in 0.0..0.5f64
    ) {
        let start = (start_frac * text.len() as f64) as usize;
        let start = start.min(text.len().saturating_sub(1));
        let del_len = (len_frac * text.len() as f64) as usize;
        let del_len = del_len.max(1);
        let end = (start + del_len).min(text.len());

        let mut rope = Rope::from_str(&text);
        rope.delete(start, end);

        let mut expected = text.clone();
        expected.replace_range(start..end, "");

        prop_assert_eq!(rope.to_string_full(), expected);
    }

    // ── Split roundtrip ────────────────────────────────────────────

    #[test]
    fn split_preserves_content(
        text in text_strategy(),
        split_frac in 0.0..1.0f64
    ) {
        let rope = Rope::from_str(&text);
        let split_pos = (split_frac * text.len() as f64) as usize;
        let split_pos = split_pos.min(text.len());

        let (left, right) = rope.split(split_pos);

        let left_str = left.to_string_full();
        let right_str = right.to_string_full();
        let combined = format!("{}{}", left_str, right_str);

        prop_assert_eq!(combined, text);
    }

    #[test]
    fn split_lengths(
        text in text_strategy(),
        split_frac in 0.0..1.0f64
    ) {
        let rope = Rope::from_str(&text);
        let split_pos = (split_frac * text.len() as f64) as usize;
        let split_pos = split_pos.min(text.len());

        let (left, right) = rope.split(split_pos);

        prop_assert_eq!(left.len(), split_pos);
        prop_assert_eq!(right.len(), text.len() - split_pos);
    }

    // ── Substring correctness ──────────────────────────────────────

    #[test]
    fn substring_matches_str_slice(
        text in "[a-zA-Z]{5,200}",
        start_frac in 0.0..1.0f64,
        len_frac in 0.0..0.5f64
    ) {
        let rope = Rope::from_str(&text);
        let start = (start_frac * text.len() as f64) as usize;
        let start = start.min(text.len());
        let sub_len = (len_frac * text.len() as f64) as usize;
        let sub_len = sub_len.max(1);
        let end = (start + sub_len).min(text.len());

        let rope_sub = rope.substring(start, end);
        let str_sub = &text[start..end];

        prop_assert_eq!(rope_sub, str_sub);
    }

    // ── Char-at consistency ────────────────────────────────────────

    #[test]
    fn char_at_matches_string(text in "[a-zA-Z]{1,100}") {
        let rope = Rope::from_str(&text);
        let bytes = text.as_bytes();

        for (i, &b) in bytes.iter().enumerate() {
            let rope_char = rope.char_at(i);
            prop_assert_eq!(rope_char, Some(b as char), "mismatch at index {}", i);
        }
        prop_assert!(rope.char_at(text.len()).is_none());
    }

    // ── Concat associativity ───────────────────────────────────────

    #[test]
    fn concat_associative(
        a in short_text_strategy(),
        b in short_text_strategy(),
        c in short_text_strategy()
    ) {
        // (a + b) + c == a + (b + c)
        let mut rope_ab_c = Rope::from_str(&a);
        rope_ab_c.append(&b);
        rope_ab_c.append(&c);

        let mut rope_a_bc = Rope::from_str(&a);
        let bc = format!("{}{}", b, c);
        rope_a_bc.append(&bc);

        prop_assert_eq!(rope_ab_c.to_string_full(), rope_a_bc.to_string_full());
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip(text in text_strategy()) {
        let rope = Rope::from_str(&text);
        let json = serde_json::to_string(&rope).unwrap();
        let restored: Rope = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), rope.len());
        prop_assert_eq!(restored.to_string_full(), text);
    }

    // ── Line operations ────────────────────────────────────────────

    #[test]
    fn line_count_matches_newlines(text in text_strategy()) {
        let rope = Rope::from_str(&text);
        if text.is_empty() {
            prop_assert_eq!(rope.line_count(), 0);
        } else {
            let expected = text.chars().filter(|&c| c == '\n').count() + 1;
            prop_assert_eq!(rope.line_count(), expected);
        }
    }

    #[test]
    fn first_line_matches(text in "[a-zA-Z0-9]{1,50}(\\n[a-zA-Z0-9]{0,50}){0,5}") {
        let rope = Rope::from_str(&text);
        let expected_first = text.split('\n').next().map(String::from);
        prop_assert_eq!(rope.line(0), expected_first);
    }

    // ── Empty operations are noops ─────────────────────────────────

    #[test]
    fn append_empty_is_noop(text in text_strategy()) {
        let mut rope = Rope::from_str(&text);
        let len_before = rope.len();
        rope.append("");
        prop_assert_eq!(rope.len(), len_before);
        prop_assert_eq!(rope.to_string_full(), text);
    }

    #[test]
    fn prepend_empty_is_noop(text in text_strategy()) {
        let mut rope = Rope::from_str(&text);
        let len_before = rope.len();
        rope.prepend("");
        prop_assert_eq!(rope.len(), len_before);
        prop_assert_eq!(rope.to_string_full(), text);
    }

    // ── Default and From equivalence ───────────────────────────────

    #[test]
    fn from_str_equivalence(text in text_strategy()) {
        let rope1 = Rope::from_str(&text);
        let rope2: Rope = text.as_str().into();

        prop_assert_eq!(rope1.to_string_full(), rope2.to_string_full());
    }

    // ── Delete out-of-bounds is safe ───────────────────────────────

    #[test]
    fn delete_oob_is_safe(
        text in text_strategy(),
        start in 0..500usize,
        end in 0..500usize
    ) {
        let mut rope = Rope::from_str(&text);
        // Should not panic regardless of start/end values
        rope.delete(start, end);
        // Result should be a valid string
        let _ = rope.to_string_full();
    }

    // ── Substring of full range gives full text ────────────────────

    #[test]
    fn substring_full_range(text in text_strategy()) {
        let rope = Rope::from_str(&text);
        let full = rope.substring(0, rope.len());
        prop_assert_eq!(full, text);
    }
}
