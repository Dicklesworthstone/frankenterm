// Test fixture: This file SHOULD trigger the no-adhoc-regex lint
// It demonstrates what NOT to do - ad-hoc regex in a non-allowed module

use fancy_regex::Regex;

/// BAD: Ad-hoc regex for detection outside the pattern system
fn detect_something_bad(text: &str) -> bool {
    // This should be flagged by the lint
    let re = Regex::new(r"some pattern").unwrap();
    re.is_match(text).unwrap_or(false)
}

/// BAD: Another ad-hoc regex
fn check_custom_pattern(text: &str) -> Option<String> {
    // This should also be flagged
    let re = Regex::new(r"capture: (\w+)").unwrap();
    re.captures(text).ok().flatten().and_then(|caps| {
        caps.get(1).map(|m| m.as_str().to_string())
    })
}
