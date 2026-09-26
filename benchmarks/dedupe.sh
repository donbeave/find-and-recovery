#!/usr/bin/env bash
# Disposable, offline benchmark for `find-and-recovery dedupe` remote scanning.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: benchmarks/dedupe.sh [--binary PATH] [--branches N]
                            [--mode MODE] [--history-depth N] [--blob-mb N]

Build the CLI first (`cargo build --release`), then run this against generated
local bare remotes only. Dedupe preview is the CLI default; this script never
passes --execute. The current CLI has no --preview flag.

MODE is one of:
  aliases             all benchmark branches point at one commit
  linear-same-tree    different commits in one linear history, same tree
  divergent-same-tree disjoint same-tree histories with a common base
  distinct-trees      different commits and different trees

Compatibility names: shared=aliases, linear=linear-same-tree,
divergent=distinct-trees, distinct=distinct-trees.
EOF
}

project_dir=$(cd "$(dirname "$0")/.." && pwd -P)
binary="$project_dir/target/release/find-and-recovery"
branches=100
mode=aliases
blob_mb=0
history_depth=1

while (($#)); do
  case "$1" in
    --binary) binary=$2; shift 2 ;;
    --branches) branches=$2; shift 2 ;;
    --mode) mode=$2; shift 2 ;;
    --history-depth) history_depth=$2; shift 2 ;;
    --blob-mb) blob_mb=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -x "$binary" ]] || { echo "binary not executable: $binary" >&2; exit 2; }
[[ "$branches" =~ ^[1-9][0-9]*$ ]] || { echo "--branches must be positive" >&2; exit 2; }
[[ "$blob_mb" =~ ^[0-9]+$ ]] || { echo "--blob-mb must be nonnegative" >&2; exit 2; }
[[ "$history_depth" =~ ^[1-9][0-9]*$ ]] || { echo "--history-depth must be positive" >&2; exit 2; }
case "$mode" in
  aliases|shared) fixture_mode=aliases ;;
  linear|linear-same-tree) fixture_mode=linear-same-tree ;;
  divergent-same-tree) fixture_mode=divergent-same-tree ;;
  divergent|distinct|distinct-trees) fixture_mode=distinct-trees ;;
  *) echo "--mode must be aliases, linear-same-tree, divergent-same-tree, or distinct-trees" >&2; exit 2 ;;
esac

object_storage_kib() {
  local git_dir=$1
  local stats loose packed fallback
  if stats=$(git --git-dir "$git_dir" count-objects -v 2>/dev/null); then
    loose=$(printf '%s\n' "$stats" | awk -F': ' '$1 == "size" {print $2; exit}')
    packed=$(printf '%s\n' "$stats" | awk -F': ' '$1 == "size-pack" {print $2; exit}')
    if [[ "$loose" =~ ^[0-9]+$ && "$packed" =~ ^[0-9]+$ ]]; then
      printf '%s\n' "$((loose + packed))"
      return
    fi
  fi
  # `du` can race Git maintenance and is only a fallback. Suppress its
  # transient lock/error output and report unavailable instead of failing the
  # benchmark because a size estimate could not be collected.
  if fallback=$(du -sk "$git_dir/objects" 2>/dev/null); then
    awk '{print $1}' <<<"$fallback"
  else
    printf 'unavailable\n'
  fi
}

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
git --git-dir "$remote" symbolic-ref HEAD refs/heads/main
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
tree=$(git -C "$local" rev-parse "$base^{tree}")

refs_file="$tmp/refs.tsv"
: > "$refs_file"
previous="$base"
for ((i=1; i<=branches; i++)); do
  printf -v suffix '%06d' "$i"
  ref="recovery/find-and-recovery/bench/$suffix"
  case "$fixture_mode" in
    aliases)
      oid=$base
      ;;
    linear-same-tree)
      oid=$(printf 'linear branch %s\n' "$suffix" | git -C "$local" commit-tree "$tree" -p "$previous")
      previous=$oid
      ;;
    divergent-same-tree)
      parent="$base"
      for ((level=1; level<=history_depth; level++)); do
        oid=$(printf 'divergent branch %s commit %s\n' "$suffix" "$level" | git -C "$local" commit-tree "$tree" -p "$parent")
        parent=$oid
      done
      ;;
    distinct-trees)
      git -C "$local" reset -q --hard "$base"
      printf 'branch %s\n' "$suffix" > "$local/branch-$suffix.txt"
      git -C "$local" add "branch-$suffix.txt"
      git -C "$local" commit -q -m "benchmark branch $suffix"
      oid=$(git -C "$local" rev-parse HEAD)
      ;;
  esac
  git -C "$local" push -q "$remote" "$oid:refs/heads/$ref"
  printf '%s\t%s\n' "$ref" "$oid" >> "$refs_file"
done

# Own exactly the generated benchmark refs, so preview exercises candidate
# planning. It still cannot authorize mutation because the command omits
# --execute and the CLI keeps remote deletion disabled.
{
  printf '{"schema_version":1,"remote":"%s","generated_unix":0,"roots":[],"coverage_gaps":[],"repositories":[],"recovery_ownership":[' "$remote"
  separator=
  while IFS=$'\t' read -r ref oid; do
    printf '%s{"source":"branch","name":"branch:%s","commit":"%s","remote_ref":"%s","verification":"fixture"}' "$separator" "$ref" "$oid" "$ref"
    separator=,
  done < "$refs_file"
  printf '],"deleted":[]}\n'
} > "$tmp/state/manifest.json"

# Trace2 records child process starts from the CLI and Git. `count-objects`
# reports loose and packed object storage without racing Git maintenance; `du`
# is used only as a guarded fallback.
trace="$tmp/trace.json"
timing="$tmp/time.txt"
stdout="$tmp/stdout.txt"
stderr="$tmp/stderr.txt"
remote_kb=$(object_storage_kib "$remote")
remote_branch_count=$(git --git-dir "$remote" for-each-ref --format='%(refname)' refs/heads | wc -l | tr -d ' ')
remote_commit_count=$(git --git-dir "$remote" rev-list --all --count)
set +e
if [[ "$(uname -s)" == Darwin* ]]; then
  GIT_TRACE2_EVENT="$trace" /usr/bin/time -l -o "$timing" \
    "$binary" --remote "$remote" --state "$tmp/state" dedupe \
    >"$stdout" 2>"$stderr"
elif [[ "$(uname -s)" == Linux* ]]; then
  GIT_TRACE2_EVENT="$trace" /usr/bin/time -v -o "$timing" \
    "$binary" --remote "$remote" --state "$tmp/state" dedupe \
    >"$stdout" 2>"$stderr"
else
  GIT_TRACE2_EVENT="$trace" /usr/bin/time -p -o "$timing" \
    "$binary" --remote "$remote" --state "$tmp/state" dedupe \
    >"$stdout" 2>"$stderr"
fi
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

timing_value() {
  local key=$1 file=$2
  awk -v key="$key" '
    {
      for (i = 1; i <= NF; i++) {
        if ($i == key) {
          if (i > 1 && $(i - 1) ~ /^[0-9]+([.][0-9]+)?$/) { print $(i - 1); exit }
          if (i < NF && $(i + 1) ~ /^[0-9]+([.][0-9]+)?$/) { print $(i + 1); exit }
        }
      }
    }
  ' "$file"
}

if [[ "$(uname -s)" == Linux* ]]; then
  wall_seconds=$(awk -F': ' '/Elapsed \(wall clock\) time/ {split($2, part, ":"); if (length(part) == 3) print part[1] * 3600 + part[2] * 60 + part[3]; else print part[1] * 60 + part[2]; exit}' "$timing")
  user_seconds=$(awk -F': ' '/User time \(seconds\)/ {print $2; exit}' "$timing")
  sys_seconds=$(awk -F': ' '/System time \(seconds\)/ {print $2; exit}' "$timing")
else
  wall_seconds=$(timing_value real "$timing")
  user_seconds=$(timing_value user "$timing")
  sys_seconds=$(timing_value sys "$timing")
fi
peak_rss=unavailable
if [[ "$(uname -s)" == Darwin* ]]; then
  peak_rss=$(awk '/maximum resident set size/ {for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+$/) value = $i; if (value != "") print value; exit}' "$timing")
  [[ "$peak_rss" =~ ^[0-9]+$ ]] || peak_rss=unavailable
elif [[ "$(uname -s)" == Linux* ]]; then
  peak_rss_kib=$(awk -F': ' '/Maximum resident set size \(kbytes\)/ {print $2; exit}' "$timing")
  if [[ "$peak_rss_kib" =~ ^[0-9]+$ ]]; then
    peak_rss=$((peak_rss_kib * 1024))
  fi
fi

printf 'metric\tvalue\n'
printf 'git\t%s\n' "$git_version"
printf 'binary\t%s\n' "$binary"
printf 'fixture_remote\tlocal bare repository (temporary)\n'
printf 'branch_count\t%s\n' "$remote_branch_count"
printf 'benchmark_branch_count\t%s\n' "$branches"
printf 'history_mode\t%s\n' "$fixture_mode"
printf 'history_depth_per_branch\t%s\n' "$history_depth"
printf 'reachable_commit_count\t%s\n' "$remote_commit_count"
printf 'large_blob_mib\t%s\n' "$blob_mb"
printf 'remote_objects_kib\t%s\n' "$remote_kb"
printf 'git_child_processes\t%s\n' "$children"
printf 'cli_stdout_bytes\t%s\n' "$output_bytes"
printf 'wall_seconds\t%s\n' "${wall_seconds:-unavailable}"
printf 'user_seconds\t%s\n' "${user_seconds:-unavailable}"
printf 'sys_seconds\t%s\n' "${sys_seconds:-unavailable}"
printf 'peak_rss_bytes\t%s\n' "$peak_rss"
printf 'preview_output_begin\n'
cat "$stdout"
printf 'preview_output_end\n'
