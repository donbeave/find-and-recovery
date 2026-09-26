# find-and-recovery

Reusable Rust CLI for finding local Git clones, bare repositories, and linked worktrees by remote URL. It can push local branch tips and detached worktree HEADs to new `recovery/` branches. It never pushes over `main`, `master`, or the configured default branch.

```sh
cargo run -- --remote https://github.com/OWNER/REPOSITORY scan --roots /Users,/Volumes,/tmp,/private/tmp
cargo run -- --remote https://github.com/OWNER/REPOSITORY preserve
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview
cargo run -- --remote https://github.com/OWNER/REPOSITORY cleanup --execute
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview --branches-only
cargo run -- --remote https://github.com/OWNER/REPOSITORY dedupe --execute
```

State defaults to `~/.local/share/find-and-recovery`; pass `--state PATH` to isolate a run. `scan` records candidate repositories and inaccessible scan paths in `manifest.json`. `preserve` checks local-only commit contents and working files for secrets, then pushes branch tips, detached worktree HEADs, stashes, unreachable commits, and snapshots of staged and working tree content without force. Push collisions and failures block cleanup. Ignored files, LFS payloads, nested repositories, shared storage, and unsupported Git dependencies block cleanup unless the inventory and preservation code explicitly supports them.

Cleanup processes repositories serially, rechecks local refs and registered worktrees, removes linked worktrees first, then removes only the exact owning clone path. Every cleanup invocation, including `cleanup --branches-only`, requires complete preservation and script-computed isolated verification of all saved refs. `preserve --branches-only` only pushes branch tips; it never authorizes deletion. Incomplete inventories, unpreserved dirty/staged, untracked, ignored, stashed, detached, unreachable, or non-branch-ref state, shared storage, unknown LFS payloads, missing or foreign worktree registrations, nested repositories, or changed state block cleanup. Cleanup rechecks local refs, worktree registrations, repository identity, and remote evidence immediately before removal.

`dedupe` takes a read-only remote snapshot and prints the configured duplicate plan. It never deletes remote branches. `dedupe --execute` fails before any remote access because remote branch deletion is disabled. Remote commits, refs, tags, and repositories are never changed by dedupe.

`dedupe` prints exact-commit groups, retained branches, and eligible exact-OID aliases for review. Same-tree branches with different commit IDs are not duplicates. `dedupe --execute` fails closed before any remote operation; this tool never deletes remote branches, tags, or repositories.
