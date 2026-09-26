# find-and-recovery

Reusable Rust CLI for finding local Git clones, bare repositories, and linked worktrees by remote URL. It can push local branch tips and detached worktree HEADs to new `recovery/` branches. It never pushes over `main`, `master`, or the configured default branch.

```sh
cargo run -- --remote https://github.com/OWNER/REPOSITORY scan --roots /Users,/Volumes,/tmp,/private/tmp
cargo run -- --remote https://github.com/OWNER/REPOSITORY preserve
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview
cargo run -- --remote https://github.com/OWNER/REPOSITORY cleanup --execute
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview --branches-only
cargo run -- --remote https://github.com/OWNER/REPOSITORY cleanup --branches-only --execute
cargo run -- --remote https://github.com/OWNER/REPOSITORY dedupe --execute
```

State defaults to `~/.local/share/find-and-recovery`; pass `--state PATH` to isolate a run. `scan` records candidate repositories and inaccessible scan paths in `manifest.json`. `preserve` checks local-only commit contents and working files for secrets, then pushes branch tips, detached worktree HEADs, stashes, unreachable commits, and snapshots of staged and working tree content without force. Push collisions and failures block cleanup. Ignored files, LFS payloads, nested repositories, shared storage, and unsupported Git dependencies block cleanup unless the inventory and preservation code explicitly supports them.

Cleanup processes repositories serially, rechecks local refs and registered worktrees, removes linked worktrees first, then removes only the exact owning clone path. Default cleanup requires preservation of worktree content. `--branches-only` removes a copy after every local branch tip has a successful recovery push; it still blocks incomplete inventories, foreign worktree dependencies, and nested repositories. Missing linked worktree paths are pruned from Git's registration after the inventory recheck.

`dedupe` fetches remote branches into an isolated bare repository and groups exact matching commit IDs. It may delete only redundant recovery refs recorded as created by this run; other remote branches stay untouched. It always protects `main`, `master`, and the advertised default branch. It rechecks both tips immediately before deleting a duplicate with `git push --delete`; it never force-pushes, changes a branch tip, or touches tags.
