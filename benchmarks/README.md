# Dedupe performance harness

`dedupe.sh` builds a disposable local bare remote and runs the built CLI in its
default preview mode. It never passes `--execute`; no remote refs are changed.
No network remotes are used. The script puts its repositories, isolated Git
configuration, manifest, and logs beneath one fresh temporary directory and
removes only that directory on exit. It prints the complete preview and
metrics to stdout; pipe it through `tee` to retain the run output.

Build and run:

```sh
cargo build --release
benchmarks/dedupe.sh --branches 500 --mode aliases | tee aliases.txt
benchmarks/dedupe.sh --branches 500 --mode linear-same-tree | tee linear.txt
benchmarks/dedupe.sh --branches 500 --mode divergent-same-tree --history-depth 8 | tee divergent.txt
benchmarks/dedupe.sh --branches 500 --mode distinct-trees --blob-mb 64 | tee distinct.txt
```

Options: `--binary PATH` selects the executable (defaults to
`target/release/find-and-recovery`); `--branches N` controls the number of
`recovery/find-and-recovery/bench/*` refs; `--blob-mb N` adds an incompressible
blob to the base history. `--history-depth N` controls each independent history
in `divergent-same-tree` (default 1). The remote has a `main` ref and advertises
it as the default. A temporary manifest marks only generated benchmark refs
as owned, so preview can display candidates. It never passes `--execute`, and
current CLI code refuses remote deletion even when `--execute` is requested.
Compatibility names are `shared=aliases`, `linear=linear-same-tree`, and
`divergent=distinct-trees`.

The fixture modes cover the planner's proof cases:

* `aliases`: every benchmark ref points to the same commit. Exact OID equality
  proves these are aliases.
* `linear-same-tree`: refs point to successive empty commits in one linear
  history. Earlier tips are contained in later tips through actual parent
  edges, even though all trees are equal.
* `divergent-same-tree`: each ref points to a separate chain of empty commits
  rooted at the common base. Set `--history-depth` to increase each chain.
  Trees are equal, but branch histories remain incomparable.
* `distinct-trees`: each ref adds a different file. Equal commit counts or
  similar shapes do not prove containment.

The planner may remove a tip only when it has an exact-OID alias among retained
refs or is reachable from a retained tip in the full parent graph. Tree equality,
commit counts, diffs, and names are not proof. The fixture's `main` is a durable
base and the bare remote advertises it as its default branch.

The harness runs the CLI's read-only preview path and never passes
`--execute`. The active planner uses exact OID equality and reachability through
the full commit-parent graph. The preview reports every retained branch and
each candidate's expected OID, relation, survivor, survivor OID, and reason.
It does not mutate remote refs: execution remains disabled until server branch
protection and active pull request facts are verified. Use
`dedupe --all-unprotected --json` to inspect the broader all-branch scope in
machine-readable form; the benchmark uses the default tool-owned-ref scope.

The report includes total and benchmark branch counts, reachable commit count,
fixture mode, bare remote object storage size, Trace2 Git child-process count,
CLI output size, and wall/user/system seconds. On macOS it reports peak
resident bytes from `/usr/bin/time -l`; Linux uses GNU `/usr/bin/time -v` and
converts its KiB value to bytes. Other platforms report peak RSS as
`unavailable`. Object storage size comes from `git count-objects -v` and sums
loose plus packed KiB. A guarded `du` fallback handles Git versions that do not
provide those counters; a maintenance lock cannot make the benchmark fail.
Storage is fixture on-disk size, not network bytes. The harness does not
estimate transferred bytes because the remote is local. Compare repeated runs
on the same host and Git version. `dd /dev/urandom` makes large blobs expensive
to generate and keeps them poorly compressible; leave `--blob-mb` at zero for
branch-count-only runs.

This measures the current read-only planner preview. Use all four modes when
comparing changes: aliases isolates same-tip grouping,
`linear-same-tree` measures ancestor containment, `divergent-same-tree` guards
against treating equal trees as proof, and `distinct-trees` measures the
no-containment path. Record the CLI version, Git version, host, fixture mode,
and output with every result. Each invocation is one pass; it does not flush OS
caches or distinguish cold and warm runs. The CLI does not expose per-stage
timings, so the report measures the full preview command only. It does not
measure filesystem discovery, LFS transfer, network transfer bytes, cleanup,
or end-to-end restore.

## Recorded release sample

Measured 2026-09-26 with `rtk cargo build --release` (0 errors, 11 warnings)
on macOS 27.0 build 26A428, Apple M5 Max, 128 GiB RAM; Rust 1.98.1 and Git
2.55.0. Each row is one release CLI preview against a newly generated local
bare remote; each has 500 benchmark refs plus `main`. Remote object size is
fixture storage, not bytes transferred. Candidate counts come from the
preview output.

| Mode | History | Commits | Candidates | Git children | Wall (s) | User (s) | Sys (s) | Peak RSS (bytes) | Remote objects (KiB) | CLI output (bytes) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `aliases` | 1 shared commit | 1 | 500 | 9 | 0.32 | 0.05 | 0.25 | 10,567,680 | 12 | 82,720 |
| `linear-same-tree` | 500 commits in one chain | 501 | 499 | 10 | 0.46 | 0.09 | 0.34 | 10,698,752 | 1,864 | 117,573 |
| `divergent-same-tree` | 500 chains, depth 4 | 2,001 | 0 | 10 | 0.51 | 0.09 | 0.42 | 11,567,104 | 1,392 | 44,220 |
| `distinct-trees` | 500 distinct tips | 501 | 0 | 10 | 0.37 | 0.07 | 0.29 | 10,846,208 | 601 | 44,220 |

These are single samples. The fixture creation warms filesystem pages before
the preview; no cache flush or controlled cold/warm pair was measured. No
before/after speedup is claimed: the earlier baseline attempt stopped at the
unset bare-remote default branch and produced no valid metrics. The reported
wall/RSS/child counts cover the complete dedupe preview command only; there is
no per-stage timing, and the local remote gives no network-transfer byte count.
