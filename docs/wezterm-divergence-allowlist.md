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
  "entries": [
    {
      "input_pattern": "tc-osc8-hyperlink-001",
      "frame_pattern": "frame-000",
      "rationale": "Known current OSC 8 hyperlink glyph/style divergence from upstream WezTerm, confined to the top-left hyperlink text pixels in run 25817861230 artifact 6979206611; tracked for removal under ft-tf6g3.55.",
      "bead_id": "ft-tf6g3.55",
      "max_changed_pixel_fraction": 0.001,
      "max_l_inf": 255,
      "min_ssim": 0.997
    },
    {
      "input_pattern": "tc-resize-wrap-001",
      "frame_pattern": "frame-000",
      "rationale": "Known current resize-control divergence after CSI 8;4;12t in run 25817861230 artifact 6979206611; the fixed capture rectangle exposes differing post-resize pixels and is tracked for normalization/removal under ft-tf6g3.54.",
      "bead_id": "ft-tf6g3.54",
      "max_changed_pixel_fraction": 0.19,
      "max_l_inf": 255,
      "min_ssim": 0.0007
    }
  ]
}
```
