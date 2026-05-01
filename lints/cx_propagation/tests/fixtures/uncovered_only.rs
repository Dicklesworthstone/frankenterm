// Fixture: every pub async fn is uncovered. Lint should report 3 findings.

pub async fn no_params() -> u32 { 0 }
pub async fn unrelated(x: u32, s: &str) -> u32 { let _ = (x, s); 0 }
pub async fn shadowed_name(cx: &str) -> u32 { let _ = cx; 0 }
