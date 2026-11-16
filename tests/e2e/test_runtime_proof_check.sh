#!/usr/bin/env bash
# E2E proof gate for ft-tf6g3.29.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

scripts/check-runtime-proof-soundness.sh

python3 - <<'PY'
from pathlib import Path

proof = Path("docs/proofs/runtime-proof-soundness.lean")
text = proof.read_text()

required_theorems = [
    "downstream_cannot_implement_runtime_proof",
    "runtime_proof_impl_requires_declared_type",
    "undeclared_type_cannot_implement_runtime_proof",
    "tokio_mutex_cannot_implement_runtime_proof",
    "downstream_type_cannot_implement_runtime_proof",
]

missing = [name for name in required_theorems if f"theorem {name}" not in text]
if missing:
    raise SystemExit(f"missing required theorem(s): {', '.join(missing)}")

print("runtime-proof-soundness: theorem inventory checked")
PY
