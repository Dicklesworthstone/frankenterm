# RuntimeProof Soundness Model

**Bead:** `ft-tf6g3.29`  
**Proof file:** `docs/proofs/runtime-proof-soundness.lean`  
**Checker:** `scripts/check-runtime-proof-soundness.sh`

## Claim

The Rust `RuntimeProof` seal is sound under the model used here:

1. `RuntimeProof` is public but has a private supertrait,
   `runtime_proof::sealed::Sealed`.
2. A downstream crate can name public traits, but it cannot name private
   traits owned by `frankenterm-core`.
3. A type can implement `RuntimeProof` only if it can also implement the
   private supertrait and is in the declared implementation set.

The Lean theorem `downstream_cannot_implement_runtime_proof` proves that no
downstream crate can implement `RuntimeProof` for any modeled type. The theorem
`undeclared_type_cannot_implement_runtime_proof` proves that even inside the
model, a type outside the declared implementation set cannot satisfy
`RuntimeProof`.

## Model Scope

This is an intentionally small model of the Rust rule FrankenTerm depends on,
not a full Rust semantics. It models only the pieces needed by the sealed-trait
argument:

- crates, represented as `frankentermCore` and `downstream`;
- trait visibility, represented as `public` and `private`;
- the public `RuntimeProof` trait and the private `sealed::Sealed` supertrait;
- the exact list of runtime proof implementation names currently present in
  `crates/frankenterm-core/src/runtime_proof.rs`;
- two negative canaries: `tokio::sync::Mutex` and a downstream type.

The companion Rust test
`crates/frankenterm-core/tests/runtime_proof_soundness_model.rs` compares the
Lean model's declared implementation list against the live Rust implementation
list so the proof cannot silently drift from the code.

## Theorems

| Theorem | Meaning |
|---------|---------|
| `downstream_cannot_name_private_sealed` | A downstream crate cannot name the private `sealed::Sealed` trait. |
| `downstream_cannot_implement_sealed` | A downstream crate cannot implement the private seal. |
| `downstream_cannot_implement_runtime_proof` | A downstream crate cannot implement `RuntimeProof`. |
| `runtime_proof_impl_requires_declared_type` | Every modeled `RuntimeProof` implementation is in the declared implementation set. |
| `undeclared_type_cannot_implement_runtime_proof` | A type outside the declared implementation set cannot implement `RuntimeProof`. |
| `tokio_mutex_cannot_implement_runtime_proof` | The modeled Tokio mutex can never satisfy `RuntimeProof`. |
| `downstream_type_cannot_implement_runtime_proof` | A modeled downstream type can never satisfy `RuntimeProof`. |

## Verification

Run:

```bash
scripts/check-runtime-proof-soundness.sh
```

The checker requires `lean`, rejects proof-hole escape hatches in the Lean
source, and type-checks the proof. CI installs Lean through `elan` and runs the
same script in the generated-artifacts job.

For the Rust-side consistency check, run:

```bash
cargo test -p frankenterm-core --test runtime_proof_soundness_model
```

## Residual Risk

The proof assumes the Rust sealed-trait pattern behaves as documented: a crate
cannot implement a public trait whose required private supertrait is not
nameable outside the defining crate. The existing Rust compile-fail doctest in
`runtime_proof.rs` remains the concrete compiler canary for that assumption;
this Lean model records and checks the project-level soundness argument around
that compiler behavior.
