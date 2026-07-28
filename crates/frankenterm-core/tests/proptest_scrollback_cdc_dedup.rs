//! M4 (round-4 Alien Optimization Gauntlet): byte-equivalence proof for
//! content-defined-chunking (CDC) dedup of warm scrollback pages before zstd.
//!
//! Proves the gated `scrollback.cdc_dedup` path is byte-identical to the legacy
//! standalone-zstd representation: a warm page's decoded lines must match the
//! legacy page's decoded lines exactly, over a diverse "capture corpus"
//! (repeated prompts/redraws, unicode, embedded newlines, empty + long lines),
//! including under eviction. Also proves the dedup actually saves bytes on
//! repeated content, and that default mode is unchanged (plain, gate off).
//!
//! Integration test ⇒ the lib links in *normal* mode, so the library's
//! `#[cfg(test)]` unit modules are not compiled — the proof is isolated from
//! concurrent sibling `#[cfg(test)]` churn elsewhere in `frankenterm-core`
//! during the round-4 campaign.

use frankenterm_core::byte_compression::CompressionLevel;
use frankenterm_core::scrollback_tiers::{ScrollbackConfig, TieredScrollback};

/// All resident warm pages decoded, newest-first.
fn warm_dump(sb: &TieredScrollback) -> Vec<Vec<String>> {
    (0..sb.warm_page_count())
        .map(|i| sb.warm_page_lines(i).expect("warm page must decode"))
        .collect()
}

/// Resident (warm oldest→newest, then hot) lines, decoded.
fn resident_lines(sb: &TieredScrollback) -> Vec<String> {
    let mut out = Vec::new();
    for i in (0..sb.warm_page_count()).rev() {
        out.extend(sb.warm_page_lines(i).expect("warm page must decode"));
    }
    out.extend(sb.tail(sb.hot_len()).into_iter().map(|s| s.to_string()));
    out
}

#[test]
fn cdc_round_trip_byte_identical_over_capture_corpus() {
    let config = ScrollbackConfig {
        hot_lines: 16,
        page_size: 8,
        warm_max_bytes: usize::MAX, // eviction off path → every page stays warm
        compression: CompressionLevel::Fast,
        cold_eviction_enabled: false,
    };
    let mut cdc = TieredScrollback::new_with_options(config.clone(), false, true);
    let mut legacy = TieredScrollback::new_with_options(config, false, false);
    assert!(cdc.cdc_stats().is_some(), "cdc arm must have a chunk store");
    assert!(legacy.cdc_stats().is_none(), "legacy arm must be plain");

    let prompt = "user@host:~/project$ ";
    for i in 0..500usize {
        let lines = [
            format!("{prompt}run task {}", i % 7),
            "=== redraw banner: status OK ===".to_string(),
            format!(
                "output {i}: unicode ✓ café ★ 日本語 — padded to a realistic terminal width here"
            ),
            if i % 5 == 0 {
                "multi\nline\nembedded\r\ncontent".to_string()
            } else {
                format!("line {i}")
            },
            if i % 11 == 0 {
                String::new()
            } else {
                "tail".to_string()
            },
        ];
        for l in lines {
            cdc.push_line(l.clone());
            legacy.push_line(l);
        }
    }

    assert_eq!(cdc.hot_len(), legacy.hot_len());
    assert_eq!(cdc.warm_page_count(), legacy.warm_page_count());
    assert_eq!(cdc.total_line_count(), legacy.total_line_count());
    assert!(cdc.warm_page_count() > 8, "must exercise many warm pages");

    // The core proof: decompress(cdc(page)) == decompress(plain(page)) == page.
    assert_eq!(
        warm_dump(&cdc),
        warm_dump(&legacy),
        "every warm page must decode byte-identically to the legacy path"
    );
    assert_eq!(
        cdc.tail(cdc.hot_len()),
        legacy.tail(legacy.hot_len()),
        "hot tier content must match"
    );

    // Accounting invariant: warm_bytes == live content-addressed-store bytes.
    let (chunks, bytes) = cdc.cdc_stats().unwrap();
    assert!(chunks > 0 && bytes > 0);
    assert_eq!(cdc.warm_total_bytes(), bytes);
}

#[test]
fn cdc_dedup_saves_bytes_on_repeated_content() {
    let config = ScrollbackConfig {
        hot_lines: 8,
        page_size: 8,
        warm_max_bytes: usize::MAX,
        compression: CompressionLevel::Fast,
        cold_eviction_enabled: false,
    };
    let mut cdc = TieredScrollback::new_with_options(config.clone(), false, true);
    let mut legacy = TieredScrollback::new_with_options(config, false, false);

    let block: Vec<String> = (0..40)
        .map(|i| format!("identical repeated prompt + output content row {i} — stable bytes here"))
        .collect();
    for _ in 0..16 {
        for l in &block {
            cdc.push_line(l.clone());
            legacy.push_line(l.clone());
        }
    }

    assert_eq!(
        warm_dump(&cdc),
        warm_dump(&legacy),
        "dedup must stay byte-identical"
    );
    assert!(
        cdc.warm_total_bytes() < legacy.warm_total_bytes(),
        "dedup must shrink warm bytes on repeats: cdc={} legacy={}",
        cdc.warm_total_bytes(),
        legacy.warm_total_bytes()
    );
}

#[test]
fn cdc_eviction_preserves_resident_pages_and_accounting() {
    // Under a tight warm cap with eviction, CDC's smaller warm_bytes keeps more
    // pages warm than legacy, so page-count parity does NOT hold (that is the
    // intended memory win). Instead verify byte-identity against a ground-truth
    // reconstruction: every resident line must equal what was pushed, proving
    // the CAS refcount frees only chunks no resident page still references.
    let config = ScrollbackConfig {
        hot_lines: 10,
        page_size: 5,
        warm_max_bytes: 300, // tight → forces cold eviction
        compression: CompressionLevel::Fast,
        cold_eviction_enabled: true,
    };
    let mut cdc = TieredScrollback::new_with_options(config, false, true);

    let mut pushed = Vec::new();
    for i in 0..600usize {
        let l = format!(
            "evt {} :: {}",
            i % 9,
            if i % 3 == 0 { "REPEATED-PAYLOAD" } else { "x" }
        );
        cdc.push_line(l.clone());
        pushed.push(l);
    }

    assert!(cdc.cold_page_count() > 0, "must have evicted to cold");
    assert_eq!(cdc.total_line_count() as usize, pushed.len());
    let evicted = cdc.cold_line_count() as usize;
    assert_eq!(
        resident_lines(&cdc).as_slice(),
        &pushed[evicted..],
        "resident lines must reconstruct byte-identically under eviction"
    );

    // No chunk leak / double-free: warm_bytes tracks the live store exactly.
    let (_chunks, bytes) = cdc.cdc_stats().unwrap();
    assert_eq!(cdc.warm_total_bytes(), bytes);

    // Clear empties the store; it is reusable afterward.
    cdc.clear();
    assert_eq!(cdc.cdc_stats(), Some((0, 0)));
    assert_eq!(cdc.warm_total_bytes(), 0);
}

#[test]
fn cdc_default_mode_is_plain() {
    // new() honors the env gate; with it unset, CDC is off (plain pages).
    if std::env::var_os("FT_MOONSHOT_SCROLLBACK_CDC_DEDUP").is_none() {
        let sb = TieredScrollback::new(ScrollbackConfig::default());
        assert!(sb.cdc_stats().is_none(), "cdc dedup must default off");
    }
}
