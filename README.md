# find-and-recovery

Reusable Rust CLI for discovering local Git copies by repository URL and recording preservation evidence.

## Commands

```sh
cargo run -- --remote https://github.com/OWNER/REPOSITORY scan --roots /Users,/Volumes,/tmp,/private/tmp
cargo run -- --remote https://github.com/OWNER/REPOSITORY preserve
cargo run -- --remote https://github.com/OWNER/REPOSITORY verify
cargo run -- --remote https://github.com/OWNER/REPOSITORY preview
```

State defaults to `~/.local/share/find-and-recovery`; pass `--state PATH` to isolate projects. Scan roots are explicit and coverage gaps are recorded in `manifest.json`. Review the manifest after each phase.

Preservation refuses repositories with unsupported dependencies or content. Verification fetches saved refs into a separate bare Git repository and compares commit and tree IDs. Do not treat a successful push as proof of preservation.

Cleanup execution is deliberately disabled. Exact filesystem, ignored-file, LFS payload, and dependency proofs are not complete. Keep every source copy until those gates exist and independent verification passes. Never remove the state directory: it contains the audit manifest.

Run the disposable fixture suite with `cargo test` before using changes.
