// Fixture: mix of covered + uncovered. Lint should report exactly the uncovered ones.

pub struct Cx;

pub async fn ok(cx: &Cx) -> u32 { let _ = cx; 0 }
pub async fn bad() -> u32 { 0 }

#[cfg(test)]
mod tests {
    // pub async fn inside #[cfg(test)] mod must NOT be inspected.
    pub async fn test_only_uncovered() -> u32 { 0 }
}

pub struct Holder;
impl Holder {
    pub async fn inherent_ok(&self, cx: &Cx) -> u32 { let _ = cx; 0 }
    pub async fn inherent_bad(&self) -> u32 { 0 }
}

async fn private_uncovered() -> u32 { 0 }
