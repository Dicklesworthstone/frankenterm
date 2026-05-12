# WezTerm Render Divergence Allowlist

Bead: `ft-tf6g3.21`

This file records deliberate FrankenTerm rendering differences from current
upstream WezTerm for the continuous differential renderer lane. The default
policy is strict: any divergent frame that does not match an entry below fails
the CI gate.

Every entry must include:

- `input_pattern`: shell-style pattern for the terminal-conformance input id.
- `frame_pattern`: shell-style pattern for the rendered frame id.
- `rationale`: why this divergence is intentional.
- `bead_id`: the bead that introduced or accepted the divergence.
- Optional metric caps (`min_ssim`, `max_l_inf`, `max_changed_pixel_fraction`)
  that bound how far the divergence may drift.

The machine-readable block below is consumed by
`scripts/wezterm-render-differential.sh`. Keep prose outside the block.

<!-- wezterm-divergence-allowlist:json -->
```json
{
  "schema_version": "wezterm-divergence-allowlist.v1",
  "entries": []
}
```
