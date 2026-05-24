#!/usr/bin/env bash
# Static guard for the StreamingRedactor anchor-coverage invariant.
#
# Bug class this fences (3 prior recurrences, all SECURITY):
#   - ft-b1p6x : azureopenai_key / devicecode / usercode collapsed key-names
#                had no STREAMING_SECRET_ANCHORS entry, so a value split across
#                a chunk boundary leaked unredacted on the streaming path.
#   - DATADOG long-form (b856249e7): DATADOG_API_KEY= form unanchored.
#   - uppercase-anchor (9ed00ce5c): uppercase env-var key-names unanchored.
#
# Invariant: every keyed-secret regex key-name that admits a *collapsed*
# (separator-elided) form via an `[_-]?` optional separator must, in its
# fully-collapsed shape, contain at least one STREAMING_SECRET_ANCHORS entry
# as a case-insensitive substring. If it does not, detect() will not fire on
# the incomplete chunk prefix AND the anchor scan
# (earliest_secret_like_suffix_start) will retain nothing — the split value
# leaks. This guard derives both the anchors and the collapsed key-name
# fragments from the source, so it cannot drift out of sync with the catalog.
#
# Static; no compilation and no RCH worker needed. Run:
#   bash tests/e2e/test_redactor_streaming_anchor_coverage.sh
#   bash tests/e2e/test_redactor_streaming_anchor_coverage.sh --self-test
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

REDACTOR="crates/frankenterm-core/src/redactor.rs"

fail() {
  printf 'redactor anchor coverage: %s\n' "$*" >&2
  exit 1
}

command -v ruby >/dev/null 2>&1 || fail "missing command: ruby"
[[ -f "${REDACTOR}" ]] || fail "missing file: ${REDACTOR}"

SELF_TEST=0
case "${1:-}" in
  --self-test) SELF_TEST=1 ;;
  "") ;;
  -h|--help)
    sed -n '2,33p' "${BASH_SOURCE[0]}"
    exit 0
    ;;
  *) fail "unknown option: $1" ;;
esac

REDACTOR_PATH="${REDACTOR}" SELF_TEST="${SELF_TEST}" ruby <<'RUBY'
src = File.read(ENV.fetch("REDACTOR_PATH"))
self_test = ENV.fetch("SELF_TEST") == "1"

def fail!(message)
  warn "redactor anchor coverage: #{message}"
  exit 1
end

# --- 1. Extract STREAMING_SECRET_ANCHORS entries from the source array. -------
anchor_block = src[/const\s+STREAMING_SECRET_ANCHORS[^=]*=\s*&\[(.*?)\];/m, 1]
fail!("could not locate STREAMING_SECRET_ANCHORS array") if anchor_block.nil?
# Strip line comments so example fragments inside `//` notes are not parsed as
# anchors (the array body interleaves explanatory comments with entries).
anchor_src = anchor_block.gsub(%r{//[^\n]*}, "")
anchors = anchor_src.scan(/"((?:\\.|[^"\\])*)"/).map { |m| m[0] }
fail!("STREAMING_SECRET_ANCHORS parsed empty") if anchors.empty?
anchors_lower = anchors.map { |a| a.downcase }

# --- 2. Extract regex literal bodies from every Regex::new(...) call. ----------
# Comments never reach this set: we only read the string passed to Regex::new,
# in each of the three literal flavors used in this file.
bodies = []
src.scan(/Regex::new\(\s*r#"(.*?)"#/m) { |m| bodies << m[0] }
src.scan(/Regex::new\(\s*r"([^"]*)"/m) { |m| bodies << m[0] }
src.scan(/Regex::new\(\s*"((?:\\.|[^"\\])*)"/m) { |m| bodies << m[0] }
fail!("no Regex::new literals found — extractor drifted") if bodies.empty?

# --- 3. Derive collapsed key-name fragments from `[_-]?` optional separators. --
# A fragment is `word(SEP word)+`, e.g. `azure[_-]?openai`, `device[_-]?code`,
# `nvidia[_-]?api`, `api[_-]?key`. Its collapsed form elides the separators:
# `azureopenai`, `devicecode`, `nvidiaapi`, `apikey`. That collapsed form is the
# literal key-name a config/env line can present (`azureopenai_key=`), and it is
# what the streaming anchor scan must be able to retain a tail for.
#
# SEP must recognize *every* spelling of the optional separator class an author
# might write. The catalog uses `[_-]?` today, but `[-_]?` (hyphen-first),
# `[_\- ]?` (escaped hyphen + space), and similar reorderings are equally
# natural Rust regex. A separator spelling the extractor fails to match is a
# silent false negative: the key-name fragment goes unseen, coverage is never
# checked, and a value split across a chunk boundary for that key-name leaks
# unredacted (ft-b1p6x class) with this guard reporting green. We discriminate a
# separator from a value char class (`[A-Za-z0-9_-]{32}`) two ways: the trailing
# `?` (value classes carry `+`/`*`/`{n,}`, never `?`) and a class body composed
# *only* of separator chars (`_`, `-`, space, with optional backslash escapes).
SEP = /\[(?:\\?[-_ ]){1,4}\]\?/
FRAGMENT = /[A-Za-z][A-Za-z0-9]*(?:#{SEP}[A-Za-z][A-Za-z0-9]*)+/
fragments = bodies.flat_map { |body| body.scan(FRAGMENT) }.uniq

if self_test
  # Tamper corpus: a fragment whose collapsed form contains NO anchor must be
  # rejected. Proves the checker enforces the invariant rather than vacuously
  # passing. The two synthetic forms also prove the separator extractor is
  # spelling-agnostic — `[_-]?` (canonical) and `[-_]?` (reversed) are both
  # collapsed and checked, so a future catalog edit using either spelling cannot
  # smuggle an unanchored key-name past the guard.
  fragments << "zzqnewprovider[_-]?vault"
  fragments << "zzqotherprovider[-_]?keyring"
end

# A fragment that contains no optional separator at all is a parser bug.
fragments.each do |frag|
  fail!("fragment #{frag.inspect} has no optional separator class (extractor drift)") unless frag.match?(SEP)
end

# --- 4. Enforce coverage: each collapsed fragment must contain an anchor. ------
uncovered = fragments.reject do |frag|
  collapsed = frag.gsub(SEP, "").downcase
  anchors_lower.any? { |a| collapsed.include?(a) }
end

if self_test
  expected = ["zzqnewprovidervault", "zzqotherproviderkeyring"].sort
  got = uncovered.map { |f| f.gsub(SEP, "").downcase }.sort
  unless got == expected
    fail!("self-test FAILED: expected exactly #{expected.inspect} uncovered, got #{got.inspect}")
  end
  puts "redactor anchor coverage: self-test passed (synthetic unanchored [_-]? and [-_]? fragments both rejected; #{fragments.length - 2} live fragments all covered)"
  exit 0
end

unless uncovered.empty?
  detail = uncovered.map { |f| "#{f} -> #{f.gsub(SEP, '').downcase}" }.join(", ")
  fail!(<<~MSG.chomp)
    #{uncovered.length} keyed-secret key-name fragment(s) have NO STREAMING_SECRET_ANCHORS coverage: #{detail}
    A value split across a chunk boundary for these key-names leaks unredacted on the streaming path (ft-b1p6x class).
    Add the collapsed key-name (or a shorter prefix of it) to STREAMING_SECRET_ANCHORS in #{ENV.fetch("REDACTOR_PATH")}.
  MSG
end

# --- 5. Registration coverage: no regex may be defined but left unwired. -------
# A `static FOO: LazyLock<Regex>` that is never referenced in SECRET_PATTERNS is
# a dead pattern — its entire secret class flows through redact()/StreamingRedactor
# unscrubbed with zero compile error. Every regex in this file is a secret
# pattern, so the invariant is exact: defined set == SECRET_PATTERNS set.
defined = src.scan(/static\s+([A-Z][A-Z0-9_]*)\s*:\s*LazyLock<Regex>/).map { |m| m[0] }.uniq.sort
pattern_block = src[/static\s+SECRET_PATTERNS[^=]*=\s*&\[(.*?)\];/m, 1]
fail!("could not locate SECRET_PATTERNS array") if pattern_block.nil?
registered = pattern_block.scan(/regex:\s*&([A-Z][A-Z0-9_]*)/).map { |m| m[0] }.uniq.sort
fail!("SECRET_PATTERNS parsed empty") if registered.empty?

orphaned = defined - registered
unless orphaned.empty?
  fail!(<<~MSG.chomp)
    #{orphaned.length} regex(es) defined but NOT registered in SECRET_PATTERNS: #{orphaned.join(", ")}
    A defined-but-unwired pattern silently leaks its entire secret class. Add a SecretPattern entry, or — if intentionally not a secret pattern — this guard's exact-equality invariant must be revisited.
  MSG
end
dangling = registered - defined
fail!("SECRET_PATTERNS references undefined regex(es): #{dangling.join(", ")}") unless dangling.empty?

# --- 6. Pattern name hygiene. -------------------------------------------------
# Each SecretPattern.name is load-bearing in three places: detect() attributes
# every match by name (`detections.push((pattern.name, ..))`), secret_pattern_names()
# exposes the list as public API, and with_debug_markers() emits `[REDACTED:{name}]`.
# A duplicate name silently merges two secret classes in attribution and the name
# list; an empty name or one carrying whitespace / `:` / `]` corrupts the debug
# marker so it can no longer be parsed back to a single pattern. Names follow the
# established snake_case convention (`api_key`, `aws_secret_key`, ...), so the
# exact invariant is: every name matches /[a-z0-9_]+/, all names are unique, and
# there is exactly one name per registered regex.
names = pattern_block.scan(/name:\s*"((?:\\.|[^"\\])*)"/).map { |m| m[0] }
fail!("no SecretPattern names parsed from SECRET_PATTERNS") if names.empty?
unless names.length == registered.length
  fail!("SECRET_PATTERNS entry drift: #{names.length} name field(s) vs #{registered.length} regex field(s) — every entry must carry exactly one of each")
end
malformed = names.reject { |n| n =~ /\A[a-z0-9_]+\z/ }
unless malformed.empty?
  fail!(<<~MSG.chomp)
    #{malformed.length} SecretPattern name(s) are not snake_case /[a-z0-9_]+/: #{malformed.inspect}
    A name with whitespace, `:`, or `]` corrupts the `[REDACTED:{name}]` debug marker and the secret_pattern_names() list.
  MSG
end
dupe_names = names.group_by(&:itself).select { |_, g| g.length > 1 }.keys.sort
unless dupe_names.empty?
  fail!(<<~MSG.chomp)
    duplicate SecretPattern name(s) in SECRET_PATTERNS: #{dupe_names.inspect}
    Detection attribution (detect() -> (name, start, end)) and secret_pattern_names() silently merge the colliding classes.
  MSG
end

# --- 7. Golden catalog pin: silent catalog shrinkage is a security regression. -
# Steps 5-6 catch a regex defined-but-unwired or a name collision, but deleting
# a whole pattern — both the `static FOO: LazyLock<Regex>` AND its SECRET_PATTERNS
# entry, together — passes every check above: the counts simply drop and that
# secret class stops being scrubbed with no compile error and no guard failure.
# Pin the exact pattern-name set as a golden so any catalog change (add OR
# remove) is a deliberate, reviewed edit. To intentionally change the catalog,
# update GOLDEN_NAMES below to match the new set; the failure message lists the
# precise delta.
GOLDEN_NAMES = %w[
  ai_provider_keyed_value anthropic_key anyscale_key aws_access_key_id
  aws_secret_key bearer_token database_url datadog_api_key device_code
  generic_api_key generic_password generic_secret generic_token
  github_fine_grained_pat github_token gitlab_token google_api_key
  google_oauth_token groq_key huggingface_token jwt_token oauth_url
  openai_key perplexity_key pgp_block replicate_token sendgrid_key
  slack_token ssh_private_key stripe_key twilio_account_sid xai_key
].sort
current_names = names.sort
unless current_names == GOLDEN_NAMES
  added = current_names - GOLDEN_NAMES
  removed = GOLDEN_NAMES - current_names
  fail!(<<~MSG.chomp)
    SECRET_PATTERNS catalog drift vs golden: added=#{added.inspect} removed=#{removed.inspect}
    A removed pattern silently stops scrubbing its entire secret class (no compile error). If this change is intentional, update GOLDEN_NAMES in this guard to match the new catalog.
  MSG
end

puts "redactor anchor coverage: passed (#{anchors.length} anchors, #{fragments.length} collapsible key-name fragments all anchored, #{registered.length} regexes all registered, #{names.length} pattern names unique + well-formed, catalog matches #{GOLDEN_NAMES.length}-entry golden)"
RUBY
