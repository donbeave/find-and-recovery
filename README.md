# find-and-recovery

Reusable Rust CLI for finding local Git copies by configured repository URL, preserving local branches and worktree snapshots on new recovery refs, and removing only a clone whose recorded pushes succeeded. Git hooks remain enabled on every push so Git LFS pre-push hooks upload payloads.

```sh
cargo run -- --remote https://github.com/OWNER/REPOSITORY scan --roots /Users,/Volumes,/tmp,/private/tmp
cargo run -- --remote https://github.com/OWNER/REPOSITORY preserve
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview
cargo run -- --remote https://github.com/OWNER/REPOSITORY cleanup --execute
```

State defaults to `~/.local/share/find-and-recovery`; use `--state PATH` to isolate projects. `scan` records inaccessible paths and coverage gaps in `manifest.json`. Branches named `main`, `master`, or the remote default map only to new recovery refs. Existing remote refs are never overwritten; a collision blocks that item. Gitleaks checks local-only commits and working files before upload.

Cleanup requires complete inventory and a successful push record for each local branch. It blocks ignored files, nested repositories, shared object stores, and unsafe dependencies. It rechecks local refs/worktrees immediately before removal, removes linked worktrees first, then removes the exact clone path. Cleanup never calls remote deletion.

Run the disposable fixture suite with `cargo test` before use.
