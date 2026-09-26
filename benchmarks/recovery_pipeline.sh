#!/usr/bin/env bash
# Local-only scan/preserve/verify/preview performance fixture.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: benchmarks/recovery_pipeline.sh [OPTIONS]

Options:
  --binary PATH             Release CLI (default: target/release/find-and-recovery)
  --clones N                Independent clones (default: 3)
  --refs-per-clone N        Local feature refs per clone (default: 32)
  --worktrees-per-clone N   Linked worktrees per clone (default: 2)
  --noise-dirs N            Non-repository directories to scan (default: 1000)
  --untracked-files N       Untracked files per clone/worktree (default: 2)

Creates only a fresh recover-pipeline-bench.* directory under TMPDIR. Uses a
local bare remote and runs scan, preserve, verify, and preview. It never runs
cleanup or contacts a network remote. Requires Gitleaks, Git LFS, and jq. The
isolated temp directory is removed on exit, including after an error.
EOF
}

project_dir=$(cd "$(dirname "$0")/.." && pwd -P)
binary="$project_dir/target/release/find-and-recovery"
clones=3
refs_per_clone=32
worktrees_per_clone=2
noise_dirs=1000
untracked_files=2

while (($#)); do
  case "$1" in
    --binary) binary=$2; shift 2 ;;
    --clones) clones=$2; shift 2 ;;
    --refs-per-clone) refs_per_clone=$2; shift 2 ;;
    --worktrees-per-clone) worktrees_per_clone=$2; shift 2 ;;
    --noise-dirs) noise_dirs=$2; shift 2 ;;
    --untracked-files) untracked_files=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -x "$binary" ]] || { echo "binary not executable: $binary" >&2; exit 2; }
[[ "$clones" =~ ^[1-9][0-9]*$ ]] || { echo "--clones must be positive" >&2; exit 2; }
[[ "$refs_per_clone" =~ ^[1-9][0-9]*$ ]] || { echo "--refs-per-clone must be positive" >&2; exit 2; }
[[ "$worktrees_per_clone" =~ ^[0-9]+$ ]] || { echo "--worktrees-per-clone must be nonnegative" >&2; exit 2; }
[[ "$noise_dirs" =~ ^[0-9]+$ ]] || { echo "--noise-dirs must be nonnegative" >&2; exit 2; }
[[ "$untracked_files" =~ ^[0-9]+$ ]] || { echo "--untracked-files must be nonnegative" >&2; exit 2; }

resolve_tool() {
  local name=$1 install_dir candidate
  if command -v mise >/dev/null 2>&1; then
    install_dir=$(mise where "$name" 2>/dev/null || true)
    candidate="$install_dir/$name"
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  fi
  command -v "$name"
}

gitleaks_bin=$(resolve_tool gitleaks) || { echo "gitleaks is required by preserve" >&2; exit 2; }
jq_bin=$(resolve_tool jq) || { echo "jq is required for benchmark diagnostics" >&2; exit 2; }
git lfs version >/dev/null 2>&1 || { echo "Git LFS is required by preservation inventory" >&2; exit 2; }

# Resolve mise shims before HOME is replaced by the isolated fixture home.
PATH="$(dirname "$gitleaks_bin"):$PATH"
export PATH

temp_root=$(cd "${TMPDIR:-/tmp}" && pwd -P)
tmp=$(mktemp -d "$temp_root/recover-pipeline-bench.XXXXXX")
cleanup_fixture() {
  case "$tmp" in
    "$temp_root"/recover-pipeline-bench.*) rm -rf -- "$tmp" ;;
    *) echo "refusing to remove unexpected benchmark path: $tmp" >&2; return 1 ;;
  esac
}
trap cleanup_fixture EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

home="$tmp/home"
state="$tmp/state"
root="$tmp/scan-root"
seed="$tmp/seed"
remote="$tmp/remote.git"
mkdir -p "$home" "$state" "$root/clones" "$root/worktrees" "$root/noise"
export HOME="$home" XDG_CONFIG_HOME="$home/.config"
export GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0
export GIT_ALLOW_PROTOCOL=file GIT_LFS_SKIP_SMUDGE=1 GIT_LFS_SKIP_PUSH=1
unset GIT_CONFIG GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR
unset GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE GIT_SHALLOW_FILE GIT_GRAFT_FILE
unset GIT_REPLACE_REF_BASE GIT_PREFIX GIT_TRACE2_EVENT GITLEAKS_CONFIG GITLEAKS_CONFIG_FILE GH_HOST

fixture_start=$SECONDS
git init --bare --quiet "$remote"
git --git-dir "$remote" symbolic-ref HEAD refs/heads/main
git init --quiet -b main "$seed"
git -C "$seed" config user.name "Recovery Benchmark Fixture"
git -C "$seed" config user.email "recovery-benchmark@localhost"
printf 'base tracked content\n' > "$seed/base.txt"
git -C "$seed" add base.txt
git -C "$seed" commit --quiet -m 'benchmark base'

for ((ref_index=1; ref_index<=refs_per_clone; ref_index++)); do
  printf -v suffix '%06d' "$ref_index"
  branch="bench/branch-$suffix"
  git -C "$seed" switch --quiet --create "$branch" main
  printf 'branch fixture %s\n' "$suffix" > "$seed/branch-$suffix.txt"
  git -C "$seed" add "branch-$suffix.txt"
  git -C "$seed" commit --quiet -m "benchmark branch $suffix"
  git -C "$seed" switch --quiet main
done
git -C "$seed" -c core.hooksPath=/dev/null push --all --quiet --no-verify "$remote"

allowed_repositories=()
for ((clone_index=1; clone_index<=clones; clone_index++)); do
  printf -v clone_suffix '%03d' "$clone_index"
  clone_path="$root/clones/clone-$clone_suffix"
  git clone --quiet --no-local -- "$remote" "$clone_path"
  git -C "$clone_path" config user.name "Recovery Benchmark Fixture"
  git -C "$clone_path" config user.email "recovery-benchmark@localhost"
  allowed_repositories+=(--allow-temp-repository "$clone_path")

  for ((ref_index=1; ref_index<=refs_per_clone; ref_index++)); do
    printf -v suffix '%06d' "$ref_index"
    git -C "$clone_path" branch --quiet "local-$suffix" "origin/bench/branch-$suffix"
  done

  make_dirty() {
    local worktree_path=$1 label=$2
    printf 'working tree edit %s\n' "$label" >> "$worktree_path/base.txt"
    printf 'staged version %s\n' "$label" > "$worktree_path/staged.txt"
    git -C "$worktree_path" add -- staged.txt
    printf 'unstaged version %s\n' "$label" > "$worktree_path/staged.txt"
    for ((file_index=1; file_index<=untracked_files; file_index++)); do
      printf -v file_suffix '%03d' "$file_index"
      printf 'untracked benchmark data %s %s\n' "$label" "$file_suffix" \
        > "$worktree_path/untracked-$label-$file_suffix.dat"
    done
  }
  make_dirty "$clone_path" "clone-$clone_suffix"

  for ((worktree_index=1; worktree_index<=worktrees_per_clone; worktree_index++)); do
    printf -v worktree_suffix '%02d' "$worktree_index"
    printf -v ref_suffix '%06d' "$worktree_index"
    worktree_path="$root/worktrees/clone-$clone_suffix-wt-$worktree_suffix"
    worktree_branch="benchmark-wt-$clone_suffix-$worktree_suffix"
    git -C "$clone_path" -c core.hooksPath=/dev/null worktree add --quiet \
      -b "$worktree_branch" "$worktree_path" "local-$ref_suffix"
    make_dirty "$worktree_path" "wt-$clone_suffix-$worktree_suffix"
  done
done

for ((dir_index=1; dir_index<=noise_dirs; dir_index++)); do
  printf -v dir_suffix '%06d' "$dir_index"
  noise_path="$root/noise/dir-$dir_suffix/level-a/level-b"
  mkdir -p "$noise_path"
  printf 'non-repository scan fixture %s\n' "$dir_suffix" > "$noise_path/payload.txt"
done
fixture_seconds=$((SECONDS - fixture_start))

run_stage() {
  local stage=$1
  shift
  local trace="$tmp/$stage.trace2.json"
  local timing="$tmp/$stage.time.txt"
  local stdout="$tmp/$stage.stdout"
  local stderr="$tmp/$stage.stderr"
  local status=0 children=unavailable wall=unavailable user=unavailable sys=unavailable rss=unavailable
  local output_bytes stderr_bytes

  printf 'stage_start\t%s\n' "$stage" >&2
  if [[ "$(uname -s)" == Darwin* ]]; then
    if GIT_TRACE2_EVENT="$trace" /usr/bin/time -l -o "$timing" "$@" > "$stdout" 2> "$stderr"; then
      status=0
    else
      status=$?
    fi
  elif [[ "$(uname -s)" == Linux* ]]; then
    if GIT_TRACE2_EVENT="$trace" /usr/bin/time -v -o "$timing" "$@" > "$stdout" 2> "$stderr"; then
      status=0
    else
      status=$?
    fi
  else
    if GIT_TRACE2_EVENT="$trace" /usr/bin/time -p -o "$timing" "$@" > "$stdout" 2> "$stderr"; then
      status=0
    else
      status=$?
    fi
  fi

  if [[ -f "$trace" ]]; then
    children=$(awk '/"event":"child_start"/ {n++} END {print n+0}' "$trace")
  fi
  output_bytes=$(wc -c < "$stdout" | tr -d ' ')
  stderr_bytes=$(wc -c < "$stderr" | tr -d ' ')
  if [[ "$(uname -s)" == Linux* ]]; then
    wall=$(awk -F': ' '/Elapsed \(wall clock\) time/ {split($2, part, ":"); if (length(part) == 3) print part[1] * 3600 + part[2] * 60 + part[3]; else print part[1] * 60 + part[2]; exit}' "$timing")
    user=$(awk -F': ' '/User time \(seconds\)/ {print $2; exit}' "$timing")
    sys=$(awk -F': ' '/System time \(seconds\)/ {print $2; exit}' "$timing")
    rss_kib=$(awk -F': ' '/Maximum resident set size \(kbytes\)/ {print $2; exit}' "$timing")
    if [[ "$rss_kib" =~ ^[0-9]+$ ]]; then
      rss=$((rss_kib * 1024))
    fi
  else
    wall=$(awk -v key=real '
      { for (i=1; i<=NF; i++) if ($i==key) { if (i>1 && $(i-1) ~ /^[0-9]+([.][0-9]+)?$/) print $(i-1); else if (i<NF && $(i+1) ~ /^[0-9]+([.][0-9]+)?$/) print $(i+1); exit } }
    ' "$timing")
    user=$(awk -v key=user '
      { for (i=1; i<=NF; i++) if ($i==key) { if (i>1 && $(i-1) ~ /^[0-9]+([.][0-9]+)?$/) print $(i-1); else if (i<NF && $(i+1) ~ /^[0-9]+([.][0-9]+)?$/) print $(i+1); exit } }
    ' "$timing")
    sys=$(awk -v key=sys '
      { for (i=1; i<=NF; i++) if ($i==key) { if (i>1 && $(i-1) ~ /^[0-9]+([.][0-9]+)?$/) print $(i-1); else if (i<NF && $(i+1) ~ /^[0-9]+([.][0-9]+)?$/) print $(i+1); exit } }
    ' "$timing")
    if [[ "$(uname -s)" == Darwin* ]]; then
      rss=$(awk '/maximum resident set size/ {for (i=1; i<=NF; i++) if ($i ~ /^[0-9]+$/) value=$i; if (value!="") print value; exit}' "$timing")
      [[ "$rss" =~ ^[0-9]+$ ]] || rss=unavailable
    fi
  fi

  printf '%s_status\t%s\n' "$stage" "$status"
  printf '%s_wall_seconds\t%s\n' "$stage" "${wall:-unavailable}"
  printf '%s_user_seconds\t%s\n' "$stage" "${user:-unavailable}"
  printf '%s_sys_seconds\t%s\n' "$stage" "${sys:-unavailable}"
  printf '%s_peak_rss_bytes\t%s\n' "$stage" "$rss"
  printf '%s_git_child_processes\t%s\n' "$stage" "$children"
  printf '%s_stdout_bytes\t%s\n' "$stage" "$output_bytes"
  printf '%s_stderr_bytes\t%s\n' "$stage" "$stderr_bytes"
  case "$stage" in
    preserve|verify|preview)
      while IFS= read -r result_line; do
        printf '%s_output\t%s\n' "$stage" "$result_line"
      done < "$stdout"
      ;;
  esac
  if ((status != 0)); then
    any_failed=1
    printf 'failed_stage_stderr_begin\t%s\n' "$stage" >&2
    tail -n 80 "$stderr" >&2 || true
    printf 'failed_stage_stdout_begin\t%s\n' "$stage" >&2
    head -n 80 "$stdout" >&2 || true
  fi
  printf 'stage_done\t%s\tstatus=%s\n' "$stage" "$status" >&2
  return 0
}

git_version=$(git --version)
rust_version=$(rustc --version 2>/dev/null || echo unavailable)
gitleaks_version=$("$gitleaks_bin" version 2>/dev/null || echo unavailable)
jq_version=$("$jq_bin" --version 2>/dev/null || echo unavailable)
lfs_version=$(git lfs version 2>/dev/null || echo unavailable)
host_os=$(uname -srm)
host_cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m)
host_memory=$(sysctl -n hw.memsize 2>/dev/null || awk '/MemTotal/ {print $2 " KiB"}' /proc/meminfo 2>/dev/null || echo unavailable)
binary_hash=$(shasum -a 256 "$binary" 2>/dev/null | awk '{print $1}')
[[ -n "$binary_hash" ]] || binary_hash=$(sha256sum "$binary" 2>/dev/null | awk '{print $1}')

printf 'metric\tvalue\n'
printf 'started_utc\t%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
printf 'binary\t%s\n' "$binary"
printf 'binary_sha256\t%s\n' "${binary_hash:-unavailable}"
printf 'rust\t%s\n' "$rust_version"
printf 'git\t%s\n' "$git_version"
printf 'gitleaks\t%s\n' "$gitleaks_version"
printf 'jq\t%s\n' "$jq_version"
printf 'git_lfs\t%s\n' "$lfs_version"
printf 'host_os\t%s\n' "$host_os"
printf 'host_cpu\t%s\n' "$host_cpu"
printf 'host_memory_bytes_or_platform_value\t%s\n' "$host_memory"
printf 'fixture_remote\tlocal bare repository; file protocol only\n'
printf 'fixture_clones\t%s\n' "$clones"
printf 'fixture_refs_per_clone\t%s\n' "$refs_per_clone"
printf 'fixture_target_remote_branch_count\t%s\n' "$((refs_per_clone + 1))"
printf 'fixture_local_heads_per_clone\t%s\n' "$((refs_per_clone + worktrees_per_clone + 1))"
printf 'fixture_total_local_heads\t%s\n' "$((clones * (refs_per_clone + worktrees_per_clone + 1)))"
printf 'fixture_linked_worktrees\t%s\n' "$((clones * worktrees_per_clone))"
printf 'fixture_noise_directory_groups\t%s\n' "$noise_dirs"
printf 'fixture_untracked_files_per_worktree\t%s\n' "$untracked_files"
printf 'fixture_total_untracked_files\t%s\n' "$((clones * (worktrees_per_clone + 1) * untracked_files))"
printf 'fixture_expected_unique_repositories\t%s\n' "$clones"
printf 'fixture_setup_seconds\t%s\n' "$fixture_seconds"

any_failed=0
allow_args=()
allow_args=("${allowed_repositories[@]}")
run_stage scan "$binary" --remote "$remote" --state "$state" scan --roots "$root" "${allow_args[@]}"
if [[ -f "$state/manifest.json" ]]; then
  discovered_repositories=$(awk '/"kind":/ {n++} END {print n+0}' "$state/manifest.json")
else
  discovered_repositories=0
fi
printf 'scan_discovered_repositories\t%s\n' "$discovered_repositories"
[[ "$discovered_repositories" == "$clones" ]] || any_failed=1

run_stage preserve "$binary" --remote "$remote" --state "$state" preserve
if [[ -f "$state/manifest.json" ]]; then
  while IFS= read -r diagnostic_line; do
    printf 'preserve_diagnostic\t%s\n' "$diagnostic_line"
  done < <("$jq_bin" -r '
    .repositories[] |
    ["path=" + .path,
     "state=" + .preservation,
     "error=" + (.verification_error // ""),
     "details=" + ((.preservation_errors // []) | join(" | "))] | @tsv
  ' "$state/manifest.json")
fi
if [[ -f "$state/manifest.json" ]]; then
  preserved_repositories=$(awk '/"preservation": "complete"/ {n++} END {print n+0}' "$state/manifest.json")
  saved_remote_refs=$(awk '/"remote_ref":/ {n++} END {print n+0}' "$state/manifest.json")
else
  preserved_repositories=0
  saved_remote_refs=0
fi
printf 'preserve_complete_repositories\t%s\n' "$preserved_repositories"
printf 'preserve_saved_remote_refs\t%s\n' "$saved_remote_refs"
[[ "$preserved_repositories" == "$clones" ]] || any_failed=1

run_stage verify "$binary" --remote "$remote" --state "$state" verify
verified_repositories=$(awk -F '\t' '$1 == "isolated-verified" {n++} END {print n+0}' "$tmp/verify.stdout")
printf 'verify_isolated_repositories\t%s\n' "$verified_repositories"
[[ "$verified_repositories" == "$clones" ]] || any_failed=1

run_stage preview "$binary" --remote "$remote" --state "$state" preview
preview_repositories=$(awk -F '\t' 'NF >= 2 {n++} END {print n+0}' "$tmp/preview.stdout")
printf 'preview_reported_repositories\t%s\n' "$preview_repositories"
[[ "$preview_repositories" == "$clones" ]] || any_failed=1
printf 'cleanup_invoked\tno\n'
printf 'network_remote_used\tno\n'
printf 'lfs_payloads_fixture\tnone\n'

if ((any_failed)); then
  echo "benchmark workload incomplete; inspect stage statuses and temporary diagnostics above" >&2
  exit 1
fi
