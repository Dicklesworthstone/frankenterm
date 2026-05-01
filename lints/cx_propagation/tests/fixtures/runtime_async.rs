// Fixture: filename is in EXEMPT_FILES. Even uncovered pub async fns
// must NOT be reported (this file is part of the seal).

pub async fn yield_now() {}
pub async fn sleep(_dur: std::time::Duration) {}
