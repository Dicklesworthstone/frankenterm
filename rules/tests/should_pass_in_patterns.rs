// Test fixture: This file simulates patterns.rs content
// It SHOULD NOT trigger the lint when patterns.rs is in the allowlist

use fancy_regex::Regex;

/// ALLOWED: Regex in the pattern engine for rule validation
fn validate_rule_regex(regex: &str) -> Result<(), String> {
    Regex::new(regex).map_err(|e| format!("Invalid regex: {e}"))?;
    Ok(())
}

/// ALLOWED: Regex for pattern extraction
fn extract_with_regex(text: &str, pattern: &str) -> Option<String> {
    let re = Regex::new(pattern).ok()?;
    re.captures(text).ok().flatten().and_then(|caps| {
        caps.get(1).map(|m| m.as_str().to_string())
    })
}
