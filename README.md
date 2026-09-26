# find-and-recovery

Reusable Rust CLI for finding local Git clones, bare repositories, and linked worktrees by remote URL. It pushes each local branch tip and detached worktree HEAD to a new `recovery/` branch. `main`, `master`, and the configured default branch are never pushed over.

```sh
cargo run -- --remote https://github.com/OWNER/REPOSITORY scan --roots /Users,/Volumes,/tmp,/private/tmp
cargo run -- --remote https://github.com/OWNER/REPOSITORY preserve
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview
cargo run -- --remote https://github.com/OWNER/REPOSITORY cleanup --execute
cargo run -- --remote https://github.com/OWNER/REPOSITORY dedupe --execute
```

State defaults to `~/.local/share/find-and-recovery`; pass `--state PATH` to isolate a run. `scan` records candidate repositories and inaccessible scan paths in `manifest.json`. `preserve` checks local-only commit contents and working files for secrets, then pushes branch tips, detached worktree HEADs, stashes, unreachable commits, and snapshots of staged and working tree content without force. Push collisions and failures block cleanup. Ignored files, LFS payloads, nested repositories, shared storage, and unsupported Git dependencies block cleanup unless the inventory and preservation code explicitly supports them.

Cleanup processes repositories serially, rechecks local refs and registered worktrees, removes linked worktrees first, then removes only the exact owning clone path. Shared stores, nested repositories, incomplete inventory, and failed pushes block removal.

`dedupe` fetches remote branches into an isolated bare repository and groups exact matching commit IDs. It may delete only redundant recovery refs recorded as created by this run; unmanaged branches remain untouched. It always protects `main`, `master`, and the advertised default branch. It rechecks both tips immediately before deleting a duplicate with `git push --delete`; it never force-pushes, changes a branch tip, or touches tags.
