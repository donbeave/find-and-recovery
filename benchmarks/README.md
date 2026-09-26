# Dedupe performance harness

`dedupe.sh` builds a disposable local bare remote and runs the built CLI in its
default preview mode. It never passes `--execute`; no remote refs are changed.
No network remotes are used. The script puts its repositories, isolated Git
configuration, manifest, and logs beneath one fresh temporary directory and
removes only that directory on exit.

Build and run:

```sh
cargo build --release
benchmarks/dedupe.sh --branches 500 --mode aliases
benchmarks/dedupe.sh --branches 500 --mode linear-same-tree
benchmarks/dedupe.sh --branches 500 --mode divergent-same-tree
benchmarks/dedupe.sh --branches 500 --mode distinct-trees --blob-mb 64
```

Options: `--binary PATH` selects the executable (defaults to
`target/release/find-and-recovery`); `--branches N` controls the number of
`recovery/bench/*` refs; `--blob-mb N` adds an incompressible blob to the base
history. The remote also has a `main` ref. The manifest uses
`repositories: []`, so the run remains preview-only and cannot authorize
deletion. Compatibility names are `shared=aliases`, `linear=linear-same-tree`,
and `divergent=distinct-trees`.

The fixture modes cover the duplicate planner's important cases:

* `aliases`: every benchmark ref points to the same commit.
* `linear-same-tree`: each ref points to a different empty commit in one
  linear history, so the commit IDs differ while the tree is identical.
* `divergent-same-tree`: each ref points to a different empty commit whose
  parent is the common base, so histories are incomparable while the tree is
  identical.
* `distinct-trees`: each ref adds a different file, so the committed trees
  differ.

The current planner groups by committed tree, keeps `main` when it is in the
group, then ranks the remaining branches by reachable commit count with a
deterministic lexical tie break. Lower-ranked candidates are planned for
create-only archive refs before branch deletion.

The report includes total and benchmark branch counts, reachable commit count,
fixture mode, bare remote object storage size, Trace2 Git child-process count,
CLI output size, and wall/user/system seconds. On macOS it also reports peak
resident bytes from `/usr/bin/time -l`; other platforms report that field as
`unavailable`. Object storage size comes from `git count-objects -v` and sums
loose plus packed KiB. A guarded `du` fallback handles Git versions that do not
provide those counters; a maintenance lock cannot make the benchmark fail.
Storage is fixture on-disk size, not network bytes. The harness does not
estimate transferred bytes because the remote is local. Compare repeated runs
on the same host and Git version. `dd /dev/urandom` makes large blobs expensive
to generate and keeps them poorly compressible; leave `--blob-mb` at zero for
branch-count-only runs.

This measures the current implementation only. Use all four modes when
comparing changes: aliases isolates same-tip grouping, the two same-tree modes
exercise tree comparison, commit-count ranking, and archive/delete planning,
and distinct-trees measures the non-duplicate path.
