/goal
Implement and finish https://github.com/donbeave/find-and-recovery as a generic, high-performance Rust CLI that evacuates all recoverable local state belonging to one explicitly selected GitHub repository to that repository, independently proves remote recoverability, safely deduplicates eligible remote branches, and then removes the verified local clones, worktrees, and associated redundant storage.

This is an implementation task, not a proposal or documentation-only review. Inspect the latest code, fix defects, implement missing functionality, add regression tests, benchmark, integrate, commit, and push the completed work.

Never delete a remote repository. Never delete, overwrite, or rewrite remote main, master, the actual default branch, or any protected/explicitly retained branch. Local commits on those branches must be preserved under new recovery refs instead.

## 1. Execution policy

Use subagents aggressively for all work. Delegate first; parallelize aggressively; verify independently; integrate.

The parent agent primarily orchestrates, decomposes dependencies, assigns ownership, resolves conflicts, integrates results, runs final deterministic gates, and ensures completion. Delegate research, architecture, implementation, debugging, tests, performance analysis, review, and verification whenever possible. Actually spawn useful subagents; do not merely recommend delegation. Keep creating useful independent workstreams as discoveries arise. Avoid concurrent conflicting edits or Git mutations in the same working tree.

Explicitly select model gpt-6-luna and reasoning effort max for every subagent whenever the runtime supports those settings. Apply this to every delegated role. Do not silently substitute another model when gpt-6-luna is available. If explicit model selection is unavailable, use the strongest available equivalent configuration and continue autonomously. If max is unsupported, use the highest exposed reasoning level. Report hard runtime limitations and actual substitutions; never claim a model setting or subagent execution that was not used.

Start parallel workstreams for discovery/identity, preservation/restore, deletion safety/journaling, remote deduplication, performance, and independent adversarial testing. Add documentation and integration work as appropriate. Have independent subagents review each destructive path and challenge every preservation assumption.

Never ask the user questions or wait for clarification. Resolve ambiguity through repository evidence, independent subagent investigation, research, alternatives, and verification. Prefer reversible decisions. Re-verify critical decisions. An unprovable runtime deletion must remain blocked, but continue all other implementation and independently safe work; do not weaken a safeguard to avoid a blocker.

Commit small, meaningful, verified increments frequently and push progress regularly. Prefer one working branch. Create additional branches only for a concrete safety or workflow need. Preserve unrelated existing work; do not reset, overwrite, or silently include it. Use merge rather than rebase when integrating the current main branch.

## 2. Revalidate the existing implementation first

Record the starting commit and run the existing tests and benchmarks. The prior audit examined commit 157762ffcdee65d8c658328152b5751e79816e34; inspect subsequent changes before acting.

Investigate and fix these concrete starting points rather than assuming they are already resolved:

- src/main.rs: preserve_branches_only collects secret-scan failures but proceeds to push branch tips. No upload path may bypass a failed, missing, or incomplete scan.
- src/main.rs: ResumePartial contains a hard-coded recovery/parallax/48c59391f8d4 mapping and bypasses normal full verification and deletion safeguards. Remove this special case and replace it with the shared resumable safety path.
- Hard-coded /Users/donbeave state and deletion protections, macOS-only discovery defaults, and simplistic URL/path handling prevent generic operation.
- dedupe --execute is deliberately disabled; its active preview only considers exact-OID, manifest-associated recovery aliases. Ordinary eligible remote branches and ancestor-contained histories need real support.
- src/dedupe.rs retains a same-tree/commit-count planner. That is not proof of identical or contained commit history and must not authorize deletion.
- src/conditional_delete.rs performs remote reads followed by plain push --delete. Replace that race-prone path, not just its disabled caller.
- push_ref checks for absence and then uses a plain non-force push. That does not enforce create-only semantics against a concurrent branch creation.
- Tags, unknown refs, ignored files, stale registrations, detached-state edge cases, and external storage currently produce broad blockers. Implement safe recovery for common cases instead of declaring the goal complete while leaving them unsupported.
- src/remote_lfs.rs enumerates tip trees rather than the full historical pointer set. Audit historical and detached-history LFS coverage.
- Repeated per-ref fetches, history walks, per-blob subprocesses, serial inventory, and subprocess pipe handling need measured improvement.
- Existing benchmark documentation describes a tree-based planner inconsistent with the active CLI. Update tests and documentation to the final behavior.

Keep working safeguards. Remove or consolidate obsolete alternatives so there is one authoritative preservation, verification, and deletion policy.

## 3. Discovery and repository identity

Accept any explicitly selected GitHub repository, not a hard-coded owner or project. Support normal HTTPS/SSH forms, configured remote names, renamed repositories, and relevant URL rewrites only after resolving and verifying the actual destination. Distinguish forks and repositories sharing history. Do not treat a matching directory name, one shared commit, or a similarly named remote as ownership proof.

Discover ordinary clones, bare/mirror clones, linked worktrees, .git files, separate Git directories, detached checkouts, copies in hidden and temporary directories, and copies outside the usual Projects directory. Handle packed refs and supported modern Git storage formats through authoritative Git interfaces rather than assumptions about loose files.

Provide configurable roots and platform-appropriate defaults for macOS and Linux. Support an exhaustive mode over the accessible selected local filesystems and a separately labelled fast mode. Report skipped directories, inaccessible roots, unmounted volumes, unsupported storage, and timeouts. Never claim a whole-machine cleanup from a partial scan.

Canonicalize and deduplicate overlapping roots and filesystem identities without following symlink loops. Inspect each Git common directory once and inventory every registered worktree. Discover nested repositories without mistaking their parent directory for safe disposable content.

For folders without Git metadata, use explicit provenance or verifiable snapshot/content evidence to identify recoverable copies; report uncertain matches and retain them. Do not assume every arbitrary folder can be attributed automatically.

Use lossless path handling, NUL-delimited Git output where applicable, and argument arrays rather than shell interpolation. Test spaces, Unicode, newlines, non-UTF-8 names on supported systems, symlinks, and unusual branch names.

Build a dependency inventory for shared object stores, alternates, separate Git directories, submodules, and nested repositories. Preserve unrelated repositories and their storage dependencies. A blocked foreign dependency must not be deleted or uploaded to the selected target.

## 4. Preserve the complete recovery state

Inventory and preserve all local branch tips, divergent copies of the same branch, detached HEADs, remote-tracking-only work, every stash entry, reflog-only commits, and recoverable unreachable commits, trees, and blobs. Do not prune or garbage-collect source repositories before completing recovery.

Preserve tags including annotated tag objects, notes, replacement-ref mappings, and other meaningful refs in a documented, restorable format. Never silently discard unsupported namespaces or change original commit IDs to simplify recovery.

Preserve staged and unstaged versions separately, untracked files, executable modes, symlink targets, and recoverable metadata. Support unborn repositories and interrupted merge/rebase/cherry-pick states without resolving conflicts on the user's behalf. Preserve conflict stages and operation metadata through a recovery format when ordinary commits cannot represent them directly.

Create recoverable snapshot commits without checking out other branches, changing source branch tips, modifying the user's index, or flattening distinct staged/working versions. Make unchanged-state retries idempotent: they must not manufacture new snapshot commits and recovery refs on every run.

Classify ignored content explicitly. Preserve non-regenerable ignored files. Delete rebuildable caches only under an explicit, recorded disposal policy; do not infer disposability from names such as build, vendor, or cache alone. Secrets, large files, unsupported objects, and uncertain content must never be silently dropped or blindly uploaded.

Fail closed on secret-scanning failures, scanner unavailability, and unsupported scanning. Cached scan failures must remain failures, never become successful cache hits. Scan the actual objects and payloads to be transmitted, including historical commits and snapshots. Keep credential material out of diagnostics and reports. Prevent unexpected hooks, filters, environment variables, URL rewrites, and LFS endpoint configuration from redirecting recovery or bypassing policy. Preserve required legitimate transformations explicitly rather than executing arbitrary repository-controlled commands unnoticed.

For LFS, enumerate pointers throughout every history being preserved, including files removed from later commits, stash parents, detached histories, and recovery snapshots. Upload and independently download all required payloads; verify SHA-256 and length. Account for orphaned local payloads. A pushed pointer is not preservation of its content.

Handle GitHub storage, push, and LFS limits explicitly. Never resolve a rejection by rewriting original history or deleting local data. Preserve all independently safe content and report unresolved recovery obligations accurately.

Push to collision-safe recovery refs using server-enforced create-only semantics. Reuse an existing ref only after proving that it has the intended content. Batch safely; never use push --mirror, blanket force, or pruning to synchronize the target.

## 5. Independent proof and resumable state

Implement one shared state machine: discover -> inventory -> preserve -> independently verify -> plan cleanup/dedupe -> revalidate -> execute -> final audit.

Use per-target state directories outside deletion candidates, derived from platform conventions and canonical repository identity. Implement versioned manifests, atomic durable checkpoints, appropriate locking, and a journal that records intent and results around every irreversible operation, not only after a whole repository finishes.

For each source item record its identity, original ref/path role, object IDs or content digest, recovery destination, verification evidence, and dependency relationships. Preserve a sanitized remote recovery index and restore instructions so recovery does not depend on the original machine or a surviving local manifest.

Do not accept editable strings such as complete or isolated-verified as deletion proof. Recompute the necessary facts against the actual local state and actual remote.

Verify recovery from a fresh isolated object database and LFS store with no source alternates, local-object shortcuts, or accidental lazy fetching. Check object closure, object integrity, required original IDs, snapshot contents, metadata mappings, and LFS bytes. Batch and share work within that isolated verifier rather than creating a fresh clone for every ref.

Provide a restore operation or tested restore workflow. Demonstrate restoration after the fixture's original local repositories, metadata, and caches have been removed. Distinguish restored Git data from optional machine-specific settings.

After interruption, reconcile the journal with filesystem and remote facts and resume idempotently. Never infer deletion permission merely because .git/HEAD or .git/config is missing. Preserve evidence for partially completed worktree removal and partial push batches. Return machine-readable nonzero statuses for incomplete/blocked execution.

## 6. Correct remote branch deduplication

Implement preview and explicit execution for both tool-created recovery branches and, with an explicit all-unprotected scope, ordinary branches in the selected remote repository. It must also work after all original local clones have been removed.

Use these exact history rules:

- Equal tip OIDs are exact aliases; retain the deterministic required survivor(s).
- A branch A may be removed as history-contained only when its tip is proven reachable from a retained branch B through the real parent graph.
- Different commits with equal trees, equal diffs, matching patch IDs, equal commit counts, or similar names are not sufficient. Do not deduplicate divergent, rebased, squashed, or cherry-picked histories without exact reachability proof.

Use complete relevant commit history and disable replacement/shallow-boundary distortions when proving ancestry. Handle merges and multiple roots correctly. Unknown history means no deletion.

Compute the retained set globally before selecting deletions. Never delete every member of an alias group or retain A only through B while also deleting B without a surviving transitive proof. For A contained in B contained in C, map both removed branches directly to a retained C or durable recovery anchor.

Always retain main, master, the actual default branch, server-protected/ruleset-protected branches, configured keep patterns, and required recovery anchors. Protect active PR head/base branches by default. An inability to establish required protection/PR facts must block the affected destructive scope, not become permission to delete.

Prefer existing meaningful and protected branches over redundant recovery aliases when their history provides the required proof. Expose every candidate, survivor, expected OID, relation, and reason in the preview and JSON plan. Preserve original-name mappings when aliases disappear.

## 7. Race-safe remote mutation

Centralize remote mutation behind a restrictive API: create approved recovery refs and delete individually approved eligible branch refs. Include no remote-repository deletion operation. Validate the exact host/repository identity, ref namespace, default branch, protection facts, and plan immediately before executing mutations.

Delete candidates only with an explicit expected-OID compare-and-delete, such as an exact force-with-lease on the candidate plus its deletion refspec. This exception authorizes conditional deletion, not history rewriting. Never fall back to an unconditional delete after a lease or protection failure.

Protect recoverability if the survivor changes concurrently. Use a tested durable remote retention anchor or equivalent transaction design, not merely a final ls-remote check. Do not assume a lease on a keeper that Git treats as up-to-date is sent to the server or that --atomic alone protects no-op refs. Avoid replacing every deleted branch with another duplicate branch; use an appropriate supported recovery namespace and minimal anchors.

Use atomic batches where they genuinely provide the required guarantees. On unsupported capabilities, retain data and report the limitation rather than silently weakening semantics. Reconcile uncertain push results by reading authoritative remote state before retrying.

After deduplication, update recovery mappings and rerun coverage verification against surviving remote references. A local cleanup must not depend on a recovery branch that dedupe just removed.

## 8. Safe and complete local cleanup

Only remove an exact inventoried target whose entire required content and dependencies are remotely verified or explicitly classified as authorized disposable data.

Before deletion, refresh the complete relevant inventory, including newly created/dropped refs, reflogs, unreachable objects, stash state, file contents, ignored/untracked files, and registrations. Checking only unchanged branch names or git status is insufficient.

Use platform-correct filesystem identity checks and safe directory operations. Protect filesystem roots, actual home directories, state/journal paths, unrelated repositories, nested foreign content, and shared-store consumers. Eliminate account-specific safeguards. Do not use guessed parent-directory deletion, wildcard rm, or symlink traversal.

Coordinate mutations per shared Git store. Establish and document the required quiescence against external writers; the tool's own lock does not stop editors or other Git processes. Revalidate after slow network work and immediately before destructive actions. Use a recoverable quarantine/revalidation approach where useful, and block when the safety boundary cannot be established.

Remove verified linked worktrees in dependency order, then owning clones/bare repositories and verified exclusive storage. Safely reconcile stale registrations. All cleanup and resume entry points must use the same gates.

After success, rescan the declared scope and account for every remaining target copy or blocker. Remove the tool's temporary clones, quarantines, and source-data caches only after verified completion. The binary/configuration and a small audit record may remain; do not leave undisclosed backup clones while reporting zero local copies.

## 9. Performance and usable CLI

Keep Rust as the implementation. Prefer small reusable modules and a thin CLI; do not replace the tool with a shell/Python wrapper, daemon, UI, or unnecessary framework.

Implement bounded configurable concurrency for independent scanning, inventory, hashing, and transfers while serializing conflicting operations per shared store. Expose useful progress and resource controls.

Avoid per-directory Git processes, duplicate candidate inspections, repeated traversal of shared history, per-branch network advertisements, per-blob processes, and repeated isolated verification downloads. Use batched Git plumbing, streaming data, and correctly scoped object-ID caches. Do not let cached success or stale remote metadata authorize deletion.

Fix subprocess pipe deadlocks, unbounded buffering, missing deadlines, cancellation, and unreaped children. Honor operating-system argument limits through batching or stdin. Measure before choosing extra dependencies or a replacement Git engine.

Provide a documented noninteractive end-to-end command plus independently usable scan, preserve, verify, preview, cleanup, dedupe, and resume capabilities. Destructive execution must require explicit flags. Discovery and previews must not mutate source repositories or working files; keep generated scan objects and caches isolated. Previews must not change remote refs or delete files. Provide JSON output, meaningful exit codes, and clear coverage summaries.

Benchmark release builds on reproducible fixtures with many directories, independent clones, shared worktrees, thousands of refs, long and divergent histories, dirty files, ignored trees, large blobs, and historical LFS. Measure wall time, peak memory, subprocess/network counts, and transferred bytes where measurable. Report cold/warm results and stage breakdowns. Keep equality grouping inexpensive and avoid an unbounded pairwise ancestry subprocess design. Establish regression budgets from recorded hardware and measurements; do not invent speedup claims.

## 10. Tests, integration, and completion

Have independent subagents implement and review regression, property, integration, failure-injection, and performance tests. Use disposable local repositories/remotes for destructive testing. Never run this implementation task's cleanup against the user's real machine or delete real GitHub branches as a demonstration.

Required scenarios include all discovery forms; same-name divergent branches; local main ahead of remote; staged plus unstaged versions; multiple stashes; reflog-only/unreachable objects; tags/notes; unborn/conflicted repositories; historical LFS; ignored secrets; scanner failures; nested/foreign repositories; shared storage; unusual paths; corrupt/missing objects; permission failures; concurrent writers; ref/default/protection changes; create-only collisions; changed candidates and survivors; interrupted pushes/deletions; and repeated resume.

Explicitly test exact aliases, true ancestor containment, merge histories, incomparable histories with identical trees, protected duplicate groups, and chains whose intermediate branches are also deletion candidates. Verify that failed safety checks transmit no disallowed data and authorize no deletion.

The decisive integration test must preserve a mixed set of clones/worktrees and local-only state, verify it remotely, deduplicate safely, delete every eligible original local copy, then reconstruct every required item from the remote alone. Confirm remote main/default/protected refs are unchanged and unique histories remain reachable. A second execution must produce no unintended changes.

Run formatting, strict linting, the full applicable test suite, CLI integration checks, and benchmark regression checks. Keep routine CI fast without deleting safety coverage. Document actual prerequisites and installation/use, remove obsolete special cases and misleading examples, and independently review the final destructive command surface.

Finish with committed and pushed implementation, actual test results, before/after performance evidence, exact usage examples, and an honest remaining-limitations report. Do not claim production execution, a successful test, complete discovery, or complete recovery without evidence. Continue until the implementable goal is finished, independently verified, and no meaningful actionable implementation work remains.
