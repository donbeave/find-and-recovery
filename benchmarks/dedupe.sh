#!/usr/bin/env bash
# Disposable, offline benchmark for `find-and-recovery dedupe` remote scanning.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: benchmarks/dedupe.sh [--binary PATH] [--branches N]
                            [--mode shared|divergent] [--blob-mb N]

Build the CLI first (`cargo build --release`), then run this against generated
local bare remotes only. Dedupe preview is the CLI default; this script never
passes --execute. The current CLI has no --preview flag.
EOF
}

project_dir=$(cd "$(dirname "$0")/.." && pwd -P)
binary="$project_dir/target/release/find-and-recovery"
branches=100
mode=shared
blob_mb=0

while (($#)); do
  case "$1" in
    --binary) binary=$2; shift 2 ;;
    --branches) branches=$2; shift 2 ;;
    --mode) mode=$2; shift 2 ;;
    --blob-mb) blob_mb=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -x "$binary" ]] || { echo "binary not executable: $binary" >&2; exit 2; }
[[ "$branches" =~ ^[1-9][0-9]*$ ]] || { echo "--branches must be positive" >&2; exit 2; }
[[ "$blob_mb" =~ ^[0-9]+$ ]] || { echo "--blob-mb must be nonnegative" >&2; exit 2; }
[[ "$mode" == shared || "$mode" == divergent ]] || { echo "--mode must be shared or divergent" >&2; exit 2; }

# An isolated HOME/config prevents user Git settings or credential helpers from
# affecting this local-only run. All generated data is under this one directory.
tmp=$(mktemp -d "${TMPDIR:-/tmp}/find-recovery-dedupe-bench.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT HUP INT TERM
mkdir -p "$tmp/home" "$tmp/state"
export HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/home/.config"
export GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0
unset GIT_CONFIG GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR
remote="$tmp/remote.git"
local="$tmp/seed"
git init --bare -q "$remote"
git init -q -b main "$local"
git -C "$local" config user.name "Benchmark Fixture"
git -C "$local" config user.email "benchmark@localhost"

if ((blob_mb > 0)); then
  # Random bytes are intentionally hard to compress so blob cost is visible.
  dd if=/dev/urandom of="$local/large.bin" bs=1048576 count="$blob_mb" 2>/dev/null
  git -C "$local" add large.bin
fi
printf 'fixture base\n' > "$local/base.txt"
git -C "$local" add base.txt
git -C "$local" commit -q -m 'benchmark base'
base=$(git -C "$local" rev-parse HEAD)
git -C "$local" push -q "$remote" "HEAD:refs/heads/main"

refs_file="$tmp/refs.tsv"
: > "$refs_file"
for ((i=1; i<=branches; i++)); do
  printf -v suffix '%06d' "$i"
  ref="recovery/bench/$suffix"
  oid=$base
  if [[ "$mode" == divergent ]]; then
    git -C "$local" reset -q --hard "$base"
    printf 'branch %s\n' "$suffix" > "$local/branch-$suffix.txt"
    git -C "$local" add "branch-$suffix.txt"
    git -C "$local" commit -q -m "benchmark branch $suffix"
    oid=$(git -C "$local" rev-parse HEAD)
  fi
  git -C "$local" push -q "$remote" "$oid:refs/heads/$ref"
  printf '%s\t%s\n' "$ref" "$oid" >> "$refs_file"
done

# Minimal valid state manifest. Empty repositories mean the benchmark measures
# the remote scan/fetch without authorizing or previewing any ref deletion.
cat > "$tmp/state/manifest.json" <<EOF
{"schema_version":1,"remote":"$remote","generated_unix":0,"roots":[],"coverage_gaps":[],"repositories":[],"deleted":[]}
EOF

# Trace2 records child process starts from the CLI and Git. Timing uses the
# platform's standard time utility; du reports fetched fixture object storage.
trace="$tmp/trace.json"
timing="$tmp/time.txt"
stdout="$tmp/stdout.txt"
stderr="$tmp/stderr.txt"
remote_kb=$(du -sk "$remote/objects" | awk '{print $1}')
set +e
GIT_TRACE2_EVENT="$trace" /usr/bin/time -p -o "$timing" \
  "$binary" --remote "$remote" --state "$tmp/state" dedupe \
  >"$stdout" 2>"$stderr"
status=$?
set -e
if ((status != 0)); then
  cat "$stderr" >&2
  cat "$stdout" >&2
  echo "benchmark command failed: exit $status" >&2
  exit "$status"
fi

children=unavailable
if [[ -f "$trace" ]]; then
  children=$(awk '/"event":"child_start"/ {n++} END {print n+0}' "$trace")
fi
output_bytes=$(wc -c < "$stdout" | tr -d ' ')
git_version=$(git --version)

printf 'metric\tvalue\n'
printf 'git\t%s\n' "$git_version"
printf 'binary\t%s\n' "$binary"
printf 'fixture_remote\tlocal bare repository (temporary)\n'
printf 'branch_count\t%s\n' "$branches"
printf 'history_mode\t%s\n' "$mode"
printf 'commits\t%s\n' "$((branches + 1))"
printf 'large_blob_mib\t%s\n' "$blob_mb"
printf 'remote_objects_kib\t%s\n' "$remote_kb"
printf 'git_child_processes\t%s\n' "$children"
printf 'cli_stdout_bytes\t%s\n' "$output_bytes"
cat "$timing"
printf 'preview_output_begin\n'
cat "$stdout"
printf 'preview_output_end\n'
