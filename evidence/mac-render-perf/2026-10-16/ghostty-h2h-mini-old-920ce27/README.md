# Pinned Ghostty vs FrankenTerm, headless ingest: rerun at 920ce2787

Same host, method and pins as `../ghostty-h2h-mini-old/` (mac-mini-old M4, quiet,
Ghostty e500d414, 64 MiB corpora, 10 ABBA rounds). The receipt is
20261016T181939Z-41094; `shasum -a 256 -c SHA256SUMS` verifies it.
FrankenTerm main 920ce2787 adds db81b59b7 (a reused row wipes only the
text it holds; bulk gap fill) and a39a055b6 (one UTF-8 validation per parse call).

FT speed relative to Ghostty (above 1 = FT faster), 80x120 / 117x512, with the
earlier 6a9c781eb run in brackets:

| corpus | 80x120 | 117x512 |
|---|---|---|
| color_emoji_random | 1.37x (1.32x) | 1.44x (1.44x) |
| color_random | 0.71x (0.70x) | 0.73x (0.72x) |
| seq_lines | 0.27x (0.24x) | 0.39x (0.24x) |
| long_lines | 0.45x (0.42x) | 0.49x (0.46x) |
| unicode_mix | 0.21x (0.20x) | 0.43x (0.42x) |
| tui_repaint | 0.62x (0.61x) | 1.17x (1.14x) |

seq_lines at 117x512: FT 1.782 -> 1.096 s (-38%), where the gap before each
line is widest. Cross-session comparison of the FT arm, same host and corpora.
