# find-and-recovery

Rust CLI for locating local Git copies whose configured remotes match a supplied target, preserving supported local state under recovery refs, verifying recorded remote data, and removing eligible local clones and worktrees.

Preservation writes under `recovery/`. The CLI has no remote branch or repository deletion operation; `dedupe --execute` fails closed. The CLI does not verify repository identity, fork ownership, branch protections, rulesets, or pull request use through GitHub metadata.

## Requirements and installation

- Rust/Cargo 1.85 or newer to build or install this edition 2024 crate.
- Git available on `PATH`, plus Git credentials that can read and push to the selected remote.
- Gitleaks (`gitleaks`) available on `PATH` for preservation. A missing, failed, or incomplete secret scan blocks uploads.
- If a repository uses Git LFS: Git LFS available as `git lfs`; `gzip` is needed for gzip-compressed LFS payloads.
- Optional: `jq` for the manifest inspection commands below.
- The shell examples use POSIX shell syntax.

From a checkout, install into Cargo's binary directory:

```sh
cargo install --path . --locked
```

Or build a release binary in the checkout:

```sh
cargo build --release --locked
BIN="$PWD/target/release/find-and-recovery"
```

## Noninteractive workflow

The example uses the platform's default scan roots. Set a distinct state directory for each target and use the same path for every step. If you pass any `--roots PATH` or `--root-list FILE`, those paths replace the defaults; include every path you want scanned. Keep state outside candidate directories.

```sh
set -eu

BIN="${CARGO_HOME:-$HOME/.cargo}/bin/find-and-recovery"
REMOTE='https://github.com/OWNER/REPOSITORY.git'
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/find-and-recovery/selected-repository"

run() {
  "$BIN" --remote "$REMOTE" --state "$STATE" "$@"
}

# Scan emits JSON and saves the manifest. Check the selected scope and gaps.
run scan
jq '{roots, coverage_gaps, repositories: [.repositories[] | {path, inventory_complete, inventory_errors}]}' \
  "$STATE/manifest.json"
jq -e '.coverage_gaps | length == 0' "$STATE/manifest.json" >/dev/null

# Preserve and independently verify the recorded Git and LFS recovery data.
run preserve
run verify

# Review the printed cleanup decisions before the explicit local deletion.
run preview
run cleanup --execute
```

If cleanup is interrupted, keep the same manifest, review eligibility again, and rerun through the shared checks:

```sh
run preview
run resume --execute
```

`resume` reruns cleanup using the saved manifest and normal revalidation. It is not a durable per-operation journal and cannot recover a missing manifest. No process lock stops external Git processes or editors from writing to candidates, and simultaneous CLI runs against the same state directory are not coordinated. Stop external writers and avoid overlapping runs during preservation and cleanup.

The `jq` check only rejects known scan gaps. It does not prove that the chosen roots cover every local copy. On macOS the defaults include the home directory and common mount/temp paths such as `/Volumes` and `/tmp`; on Linux they include the home directory and common mount/temp paths such as `/mnt`, `/media`, and `/tmp`. `--exhaustive` starts at the filesystem root, but inaccessible locations and traversal errors can still leave gaps. Descendant symlink directories are not followed. Review the intended roots and the entire manifest; a gap-free scan is not proof of whole-machine discovery.

`scan` can exit successfully while listing gaps. Preservation, verification, cleanup, and `dedupe --execute` return nonzero when blocked or incomplete. Cleanup can remove eligible copies already found even when coverage gaps exist, then return an incomplete status; do not execute it until the scope is reviewed and known gaps are resolved. The manifest records candidate-specific inventory, preservation, verification, and deletion statuses.

## Commands and limits

- `scan [--roots PATH ...] [--root-list FILE] [--exhaustive]`: discover matching clones, bare repositories, and linked worktrees. `--root-list` must be NUL-delimited and end with NUL.
- `refresh --path PATH`: refresh one inventoried repository.
- `preserve`: scan and push supported recoverable state under recovery refs. `preserve --branches-only` pushes branch tips only and never authorizes cleanup.
- `verify`: independently fetch and check recorded remote recovery evidence.
- `preview`: print local cleanup decisions. `preview --branches-only` reports state omitted by branch-only preservation; it does not reduce cleanup requirements.
- `cleanup --execute` and `resume --execute`: explicitly remove eligible local paths after the common checks. They do not delete remote refs or repositories.
- `dedupe [--json]`: read remote branch history and preview the branch plan. `dedupe --all-unprotected` broadens preview scope only; protection and pull request facts are not checked. `dedupe --execute` always fails closed before remote access.
- `record-recovered-deletion --path ABSOLUTE_PATH --ref-prefix HEX`: rebuild a local deletion-history entry from known manifest history and matching remote recovery refs. It does not restore a repository.

`scan` always prints a JSON manifest and writes `$STATE/manifest.json`; inspect `coverage_gaps`, `inventory_errors`, and candidate statuses. `dedupe --json` prints a JSON plan with `execution_authorized: false` and the execution blocker. Other commands report statuses as text.

Candidate matching currently uses configured Git remote URLs and branch upstreams. It does not independently identify a GitHub repository through the GitHub API; forks or ambiguous remote configuration require manual review. Branch protection, rulesets, and open pull requests are not queried. Remote branch deletion therefore remains disabled.

Secret-scan failures block uploads. Unsupported refs or objects, unresolved shared storage, ignored or uncertain local content, incomplete worktree registrations, changed state, and unavailable or unverifiable LFS payloads can block cleanup. Preserve only sends data after its supported secret checks; do not treat scanner success as a secret-free guarantee.

There is no standalone restore command or remote recovery index. The local manifest contains source-to-recovery mappings; keep it with the recovery refs until a tested restore workflow exists. Remote verification proves the recorded data at verification time and does not prevent later changes by external actors.
