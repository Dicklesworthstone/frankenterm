# Example: Theme Extension

> **Experimental package example (ft-rxk40).** No production `.ftx` loader
> applies these assets. Theme settings supported by the existing terminal
> configuration remain a separate surface; `ft ext` installs pattern packs,
> not this theme package.

A theme-only extension that applies a custom color scheme. No code needed --
just a manifest and a colors file.

## File structure

```
dracula-plus/
  extension.toml
  assets/
    colors.toml
```

## extension.toml

```toml
[extension]
name = "dracula-plus"
version = "1.0.0"
description = "Enhanced Dracula color scheme"
authors = ["Theme Author"]
license = "MIT"

[engine]
type = "lua"
entry = "noop.lua"

[permissions]
# No permissions needed for a theme-only extension

[assets]
themes = ["assets/colors.toml"]
```

## assets/colors.toml

```toml
[colors]
foreground = "#f8f8f2"
background = "#282a36"
cursor_bg = "#f8f8f2"
cursor_fg = "#282a36"
cursor_border = "#f8f8f2"
selection_bg = "#44475a"
selection_fg = "#f8f8f2"

[colors.ansi]
black = "#21222c"
red = "#ff5555"
green = "#50fa7b"
yellow = "#f1fa8c"
blue = "#bd93f9"
purple = "#ff79c6"
cyan = "#8be9fd"
white = "#f8f8f2"

[colors.brights]
black = "#6272a4"
red = "#ff6e6e"
green = "#69ff94"
yellow = "#ffffa5"
blue = "#d6acff"
purple = "#ff92df"
cyan = "#a4ffff"
white = "#ffffff"
```

## noop.lua

```lua
-- Theme-only extension: no runtime behavior needed
```

## Experimental package artifact

```bash
cd dracula-plus
zip -r ../dracula-plus.ftx extension.toml assets/ noop.lua
# Production installation is unavailable (ft-rxk40).
```

The proposed package would expose this theme setting after loader integration;
the package currently registers no color scheme:

```toml
color_scheme = "dracula-plus"
```
