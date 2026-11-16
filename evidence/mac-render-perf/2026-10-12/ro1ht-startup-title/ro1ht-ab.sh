#!/bin/sh
# ft-ro1ht native A/B: does a title the shell sets at startup reach the window?
# Arm A = 07f454fb4 as is (new_window reads titles after subscribing).
# Arm B = the same tree without that update_title() call.
# Each run logs every format-window-title evaluation. Isolated HOME/XDG, own
# class, --always-new-process: nothing publishes or dials the operator's mux.
set -u
C=$HOME/ft-build/frankenterm-5351d
R=$HOME/ft-build/ro1ht
T=$HOME/cargo-targets/ft-5351-ro1ht
SRC=crates/frankenterm-gui/src/termwindow/mod.rs
export RCH_CARGO_WRAPPER_BYPASS=1 CARGO_TARGET_DIR=$T
cd "$C" || exit 1
[ -z "$(git status --porcelain)" ] || { echo "clone not clean"; exit 1; }

echo "build B $(date +%H:%M:%S)"
cp "$R/mod-B.rs" "$SRC"
~/.cargo/bin/cargo build -p frankenterm-gui > "$R/build-B.log" 2>&1
rcb=$?
cp "$R/mod-A.rs" "$SRC"
[ $rcb -eq 0 ] || { echo "build B failed rc=$rcb"; exit 1; }
cp "$T/debug/frankenterm-gui" "$R/gui-B"
[ -z "$(git status --porcelain)" ] || { echo "restore failed"; exit 1; }

echo "build A $(date +%H:%M:%S)"
~/.cargo/bin/cargo build -p frankenterm-gui > "$R/build-A.log" 2>&1 || { echo "build A failed"; exit 1; }
cp "$T/debug/frankenterm-gui" "$R/gui-A"

n=0
for arm in B A B A B A; do
  n=$((n + 1))
  token="ftgt-ro1ht-$arm-$n"
  D="$R/run-$n-$arm"
  mkdir -p "$D/home" "$D/config"
  cat > "$D/cfg.lua" <<'EOF'
local wezterm = require 'wezterm'
wezterm.on('format-window-title', function(tab, pane, tabs, panes, config)
  wezterm.log_info('ro1ht-title ' .. tostring(pane and pane.title))
  return nil
end)
return {}
EOF
  env -u WEZTERM_UNIX_SOCKET -u FRANKENTERM_CONFIG_FILE -u WEZTERM_CONFIG_FILE \
    HOME="$D/home" XDG_CONFIG_HOME="$D/config" FRANKENTERM_LUA_CONFIG=1 \
    RUST_LOG=lua_config=info,warn \
    "$R/gui-$arm" --config-file "$D/cfg.lua" start --always-new-process \
    --class "ro1ht-$arm-$n" -- /bin/sh -c "printf '\\033]2;$token\\007'; exec sleep 40" \
    > "$D/gui.log" 2>&1 &
  pid=$!
  sleep 30
  if kill -0 "$pid" 2>/dev/null; then kill "$pid"; fi
  wait "$pid" 2>/dev/null
  evals=$(grep -c "ro1ht-title" "$D/gui.log")
  hits=$(grep -c "ro1ht-title $token" "$D/gui.log")
  echo "run $n arm $arm: title evaluations=$evals with-token=$hits $(date +%H:%M:%S)"
done
echo done
