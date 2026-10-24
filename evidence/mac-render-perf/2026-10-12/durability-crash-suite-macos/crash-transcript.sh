cd ~/ft-build/frankenterm-5351g
export RCH_CARGO_WRAPPER_BYPASS=1 CARGO_TARGET_DIR=$HOME/cargo-targets/ft-5351-dev
echo "# ft-yccm0.2.1.7 AC4 native macOS subset: crash and recovery suite"
echo "# host: $(hostname -s) $(sw_vers -productVersion) $(uname -m); toolchain: $(~/.cargo/bin/rustc --version)"
echo "# source: 15a2b1c46 + da2c97e4b (git diff sha f20faf27eb3d) = the code of 0460bffe4"
echo "# filesystem: $(df -h ~/ft-build | tail -1 | cut -c1-40) $(diskutil info / 2>/dev/null | grep -m1 "Type (Bundle)" | sed "s/  */ /g")"
echo "# TMPDIR: ${TMPDIR:-unset}"
echo "# started $(date -u +%Y-%m-%dT%H:%M:%SZ) load $(uptime | sed "s/.*averages: //")"
~/.cargo/bin/cargo test -p frankenterm-mux-server-impl --lib -- killed crash torn digest_only reorder recover segment 2>&1 | grep -E "^test |^test result|running|panicked"
echo "# finished $(date -u +%Y-%m-%dT%H:%M:%SZ) load $(uptime | sed "s/.*averages: //")"
