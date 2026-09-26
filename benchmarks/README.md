# Dedupe performance harness

`dedupe.sh` builds a disposable local bare remote and runs the built CLI in its
default preview mode. It never passes `--execute`; the current CLI has no
`--preview` switch because preview is the default. No network remotes are used.
The script puts its repositories, isolated Git configuration, manifest, and
logs beneath one fresh temporary directory and removes only that directory on
exit.

Build and run:

```sh
cargo build --release
benchmarks/dedupe.sh --branches 500 --mode shared
benchmarks/dedupe.sh --branches 500 --mode divergent --blob-mb 64
```

Options: `--binary PATH` selects the executable (defaults to
`target/release/find-and-recovery`); `--branches N` controls the number of
`recovery/bench/*` refs; `--mode shared|divergent` makes all refs point to the
same base commit or to distinct one-commit descendants of a shared base;
`--blob-mb N` adds an incompressible blob to the shared base history. The
remote also has a `main` ref. The manifest uses `repositories: []`, so no refs
are marked as tool-managed and no delete candidates can be authorized. Shared
mode therefore measures listing/fetch/grouping overhead, not deletion output
or execution.

The report includes fixture dimensions, bare remote object storage size,
Trace2 Git child-process count when available, CLI output size, and `/usr/bin/time`
wall/user/system seconds. Object storage size is the fixture's on-disk Git
object size, not network bytes. The harness does not estimate transferred
bytes; the remote is local. Compare repeated runs on the same host and Git
version. `dd /dev/urandom` makes large blobs expensive to generate and keeps
them poorly compressible; leave `--blob-mb` at zero for branch-count-only runs.

This measures the current implementation only. In particular, the current
dedupe implementation groups equal commit IDs and reads branch refs one at a
time after fetching. It does not yet compare equal trees at different commit
IDs or select the longer history. See the project review before interpreting
results as a benchmark of those not-yet-implemented semantics.
