# find-and-recovery

Reusable Rust CLI for finding local Git copies by configured repository URL, preserving local branches and worktree snapshots on new recovery refs, and removing only a clone whose recorded pushes succeeded. Git hooks remain enabled on every push so Git LFS pre-push hooks upload payloads.

```sh
cargo run -- --remote https://github.com/OWNER/REPOSITORY scan --roots /Users,/Volumes,/tmp,/private/tmp
cargo run -- --remote https://github.com/OWNER/REPOSITORY preserve
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview
cargo run -- --remote https://github.com/OWNER/REPOSITORY cleanup --execute
cargo run -- --remote https://github.com/OWNER/REPOSITORY dedupe --execute
```

State defaults to `~/.local/share/find-and-recovery`; use `--state PATH` to isolate projects. `scan` records inaccessible paths and coverage gaps in `manifest.json`. Branches named `main`, `master`, or the remote default map only to new recovery refs. Existing remote refs are never overwritten; a collision blocks that item. Gitleaks checks local-only commits and working files before upload.

Cleanup requires complete inventory and successful pushes for every branch, stash, detached head, and dirty worktree snapshot. It blocks ignored files, nested repositories, LFS payloads that cannot be transferred, shared object stores, and unsupported refs. It rechecks local refs/worktrees immediately before removal, removes linked worktrees first, then removes the exact clone path.

After all manifest clones are deleted, `dedupe` compares remote branch tip commit IDs. It can delete only exact duplicate recovery refs created by this run. It keeps one copy when needed and never deletes `main`, `master`, or the advertised default branch. Deletion uses a normal Git ref delete; it does not force-push or overwrite a remote ref.

Run the disposable fixture suite with `cargo test` before use.
