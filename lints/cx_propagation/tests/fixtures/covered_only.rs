// Fixture: every pub async fn is covered. Lint should pass.

pub struct Cx;
pub trait RuntimeProof {}

pub async fn ref_cx(cx: &Cx) -> u32 { let _ = cx; 0 }
pub async fn mut_ref_cx(cx: &mut Cx) -> u32 { let _ = cx; 0 }
pub async fn owned_cx(cx: Cx) -> u32 { let _ = cx; 0 }
pub async fn impl_proof<R: RuntimeProof>(rt: R) -> u32 { let _ = rt; 0 }
pub async fn where_proof<R>(rt: R) -> u32 where R: RuntimeProof { let _ = rt; 0 }
