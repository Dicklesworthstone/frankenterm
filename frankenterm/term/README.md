# frankenterm-term

This crate provides FrankenTerm's virtual terminal emulator. It is maintained
in this repository and derives from the WezTerm terminal implementation.

It is full featured, providing terminal escape sequence parsing, keyboard
and mouse input encoding, a model for the screen cells including scrollback,
sixel and iTerm2 image support, OSC 8 Hyperlinks and a wide range of
terminal cell attributes.

This crate does not provide any kind of gui, nor does it directly
manage a PTY; you provide a `std::io::Write` implementation that
could connect to a PTY, and supply bytes to the model via the
`advance_bytes` method.

The entrypoint to the crate is the [Terminal](src/terminal.rs)
struct.

The `byte_to_grid` benchmark includes `reflow_cpu` cases that call the real
`Terminal::resize` path with ASCII and Unicode scrollback. They measure cold
resize and repeated width changes, exclude parsing from timed resize, and
check generated text and cursor bounds. These CPU measurements do not measure
GUI presentation, font rasterization or GPU work. Run them through the strict
RCH development lane, for example:

```sh
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  cargo bench -j2 --locked -p frankenterm-term --bench byte_to_grid \
  --profile release-perf -- reflow_cpu
```

License: MIT
