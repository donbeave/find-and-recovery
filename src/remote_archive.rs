//! Create-only remote refs for preserving branch history before cleanup.
//!
//! Archive refs are keyed by the commit object ID. Exact branch aliases
//! therefore share one archival ref, while a different object can never
//! overwrite an existing archive. Source objects are fetched into a fresh
//! bare repository before the push.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Output};

/// Input to [`archive_commits_batch`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveRecord {
    pub original_ref: String,
    pub expected_oid: String,
    pub expected_tree: String,
}

/// Preserve all records under deterministic create-only archive refs.
///
/// The returned map contains one archive ref for every unique input branch.
/// Existing refs are accepted only when their tip equals `expected_oid` and
/// the commit resolves to `expected_tree` in a fresh verifier repository.
/// New refs use a normal, non-force push. Existing archive refs are accepted
/// only when their tip equals the requested commit.
pub fn archive_commits_batch(
    remote: &str,
    records: &[ArchiveRecord],
) -> Result<BTreeMap<String, String>, String> {
    let records = normalize_records(records)?;
    if records.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut all_refs_set = BTreeSet::new();
    for record in records.values() {
        all_refs_set.insert(record.original_ref.clone());
        all_refs_set.insert(archive_ref_for(&record.expected_oid));
    }
    let all_refs = all_refs_set.into_iter().collect::<Vec<_>>();
    let initial = remote_ref_oids(remote, &all_refs)?;

    let mut existing = BTreeMap::<String, String>::new();
    let mut missing = Vec::<ArchiveRecord>::new();
    for record in records.values() {
        let archive_ref = archive_ref_for(&record.expected_oid);
        match initial.get(&archive_ref) {
            Some(actual) if actual == &record.expected_oid => {
                existing.insert(record.original_ref.clone(), archive_ref);
            }
            Some(_) => {
                return Err(format!(
                    "archive ref already exists at a different commit: {archive_ref}"
                ));
            }
            None => {
                if initial.get(&record.original_ref) != Some(&record.expected_oid) {
                    return Err(format!(
                        "source branch tip does not match expected commit: {}",
                        record.original_ref
                    ));
                }
                missing.push(record.clone());
            }
        }
    }

    if !missing.is_empty() {
        let temp = tempfile::Builder::new()
            .prefix("find-recovery-archive-")
            .tempdir()
            .map_err(|error| format!("create isolated archive directory: {error}"))?;
        let git_dir = temp.path().join("objects.git");
        git_checked(
            None,
            [
                OsStr::new("init"),
                OsStr::new("--bare"),
                OsStr::new("--quiet"),
                git_path(&git_dir),
            ],
            "initialize isolated archive repository",
        )?;

        fetch_sources(remote, &git_dir, &missing)?;
        for record in &missing {
            let source_ref = source_stage_ref(&record.original_ref);
            let fetched_oid = local_ref_oid(&git_dir, &source_ref)?.ok_or_else(|| {
                format!(
                    "source ref was not fetched into the isolated repository: {}",
                    record.original_ref
                )
            })?;
            if fetched_oid != record.expected_oid {
                return Err(format!(
                    "source branch tip changed before archival: {}",
                    record.original_ref
                ));
            }
            verify_local_commit(
                &git_dir,
                &source_ref,
                &record.expected_oid,
                &record.expected_tree,
            )?;
        }

        push_missing(remote, &git_dir, &missing, &mut existing)?;
    }

    let mut result = BTreeMap::new();
    for record in records.values() {
        result.insert(
            record.original_ref.clone(),
            archive_ref_for(&record.expected_oid),
        );
    }

    // One independent repository verifies every unique archive ref after all
    // create attempts. This also verifies refs that existed beforehand.
    verify_remote_archives(remote, &records, &result)?;
    Ok(result)
}

/// Preserve one commit. This is a convenience wrapper around the batched API.
pub fn archive_commit(
    remote: &str,
    original_ref: &str,
    expected_oid: &str,
    expected_tree: &str,
) -> Result<String, String> {
    let record = ArchiveRecord {
        original_ref: original_ref.to_owned(),
        expected_oid: expected_oid.to_owned(),
        expected_tree: expected_tree.to_owned(),
    };
    archive_commits_batch(remote, &[record])?
        .into_values()
        .next()
        .ok_or_else(|| "archive operation returned no archive ref".to_owned())
}

/// Return the deterministic archive ref for one commit object ID.
///
/// Git object IDs are cryptographic content identifiers and are validated by
/// the batch API, so the OID itself is the collision-resistant namespace key.
pub fn archive_ref_for(expected_oid: &str) -> String {
    format!("refs/archive/find-and-recovery/dedupe/{expected_oid}")
}

fn normalize_records(records: &[ArchiveRecord]) -> Result<BTreeMap<String, ArchiveRecord>, String> {
    let mut normalized = BTreeMap::new();
    let mut oid_trees = BTreeMap::<String, String>::new();
    for record in records {
        validate_branch_ref(&record.original_ref)?;
        validate_oid(&record.expected_oid, "commit")?;
        validate_oid(&record.expected_tree, "tree")?;
        if let Some(previous_tree) = oid_trees.get(&record.expected_oid) {
            if previous_tree != &record.expected_tree {
                return Err(format!(
                    "same commit was submitted with conflicting trees: {}",
                    record.expected_oid
                ));
            }
        } else {
            oid_trees.insert(record.expected_oid.clone(), record.expected_tree.clone());
        }
        if let Some(previous) = normalized.get(&record.original_ref) {
            if previous != record {
                return Err(format!(
                    "duplicate archive input has conflicting expectations: {}",
                    record.original_ref
                ));
            }
            continue;
        }
        normalized.insert(record.original_ref.clone(), record.clone());
    }
    Ok(normalized)
}

fn validate_branch_ref(reference: &str) -> Result<(), String> {
    if !reference.starts_with("refs/heads/") || reference == "refs/heads/" {
        return Err(format!("expected a branch ref, got: {reference}"));
    }
    git_checked(
        None,
        [OsStr::new("check-ref-format"), OsStr::new(reference)],
        "validate branch ref",
    )?;
    Ok(())
}

fn validate_oid(oid: &str, kind: &str) -> Result<(), String> {
    if (oid.len() != 40 && oid.len() != 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid expected {kind} object ID"));
    }
    Ok(())
}

fn remote_ref_oids(
    remote: &str,
    references: &[String],
) -> Result<BTreeMap<String, String>, String> {
    if references.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut args = vec![
        OsString::from("ls-remote"),
        OsString::from("--refs"),
        OsString::from("--"),
        OsString::from(remote),
    ];
    args.extend(references.iter().cloned().map(OsString::from));
    let output = git_checked(None, args, "read remote archive refs")?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "remote ref listing is not UTF-8".to_owned())?;
    let requested = references
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut found = BTreeMap::new();
    for line in text.lines() {
        let (oid, name) = line
            .split_once('\t')
            .ok_or_else(|| "malformed remote ref listing".to_owned())?;
        if !requested.contains(name) || found.insert(name.to_owned(), oid.to_owned()).is_some() {
            return Err("remote archive ref listing was ambiguous".to_owned());
        }
    }
    Ok(found)
}

fn fetch_sources(remote: &str, git_dir: &Path, records: &[ArchiveRecord]) -> Result<(), String> {
    let mut refspecs = Vec::with_capacity(records.len());
    for record in records {
        refspecs.push(OsString::from(format!(
            "{}:{}",
            record.original_ref,
            source_stage_ref(&record.original_ref)
        )));
    }
    let mut filtered = vec![
        OsString::from("--git-dir"),
        git_dir.as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from("protocol.file.allow=always"),
        OsString::from("fetch"),
        OsString::from("--quiet"),
        OsString::from("--no-tags"),
        OsString::from("--filter=blob:none"),
        OsString::from("--"),
        OsString::from(remote),
    ];
    filtered.extend(refspecs.iter().cloned());
    if git_checked(None, filtered, "fetch source refs with blob filter").is_err() {
        let mut plain = vec![
            OsString::from("--git-dir"),
            git_dir.as_os_str().to_owned(),
            OsString::from("-c"),
            OsString::from("protocol.file.allow=always"),
            OsString::from("fetch"),
            OsString::from("--quiet"),
            OsString::from("--no-tags"),
            OsString::from("--"),
            OsString::from(remote),
        ];
        plain.extend(refspecs);
        git_checked(None, plain, "fetch source refs")?;
    }
    Ok(())
}

fn push_missing(
    remote: &str,
    git_dir: &Path,
    records: &[ArchiveRecord],
    existing: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let mut by_archive = BTreeMap::<String, ArchiveRecord>::new();
    for record in records {
        by_archive
            .entry(archive_ref_for(&record.expected_oid))
            .or_insert_with(|| record.clone());
    }
    let mut pending = by_archive.into_values().collect::<Vec<_>>();
    for attempt in 0..2 {
        if pending.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            OsString::from("--git-dir"),
            git_dir.as_os_str().to_owned(),
            OsString::from("push"),
            OsString::from("--quiet"),
            OsString::from("--no-tags"),
            OsString::from("--atomic"),
        ];
        args.push(OsString::from("--"));
        args.push(OsString::from(remote));
        for record in &pending {
            args.push(OsString::from(format!(
                "{}:{}",
                source_stage_ref(&record.original_ref),
                archive_ref_for(&record.expected_oid)
            )));
        }
        let push_result = git_checked(None, args, "create archive refs");
        let archive_refs = pending
            .iter()
            .map(|record| archive_ref_for(&record.expected_oid))
            .collect::<Vec<_>>();
        let observed = remote_ref_oids(remote, &archive_refs)?;
        let mut next = Vec::new();
        for record in pending {
            let archive_ref = archive_ref_for(&record.expected_oid);
            match observed.get(&archive_ref) {
                Some(actual) if actual == &record.expected_oid => {
                    existing.insert(record.original_ref.clone(), archive_ref);
                }
                Some(_) => {
                    return Err(format!(
                        "archive ref was created at a different commit: {archive_ref}"
                    ));
                }
                None => next.push(record),
            }
        }
        if push_result.is_ok() && next.is_empty() {
            return Ok(());
        }
        pending = next;
        if attempt == 1 {
            return Err("create archive refs failed before all refs appeared".to_owned());
        }
    }
    unreachable!("bounded archive push loop")
}

fn verify_remote_archives(
    remote: &str,
    records: &BTreeMap<String, ArchiveRecord>,
    archive_refs: &BTreeMap<String, String>,
) -> Result<(), String> {
    let temp = tempfile::Builder::new()
        .prefix("find-recovery-archive-verify-")
        .tempdir()
        .map_err(|error| format!("create isolated archive verification directory: {error}"))?;
    let git_dir = temp.path().join("objects.git");
    git_checked(
        None,
        [
            OsStr::new("init"),
            OsStr::new("--bare"),
            OsStr::new("--quiet"),
            git_path(&git_dir),
        ],
        "initialize archive verification repository",
    )?;

    let mut unique = BTreeMap::<String, ArchiveRecord>::new();
    for record in records.values() {
        unique
            .entry(archive_ref_for(&record.expected_oid))
            .or_insert_with(|| record.clone());
    }
    let mut refspecs = Vec::with_capacity(unique.len());
    let mut destinations = BTreeMap::new();
    for (archive_ref, record) in &unique {
        let suffix = archive_ref
            .strip_prefix("refs/archive/find-and-recovery/dedupe/")
            .ok_or_else(|| "malformed archive ref result".to_owned())?;
        let destination = format!("refs/archive/find-and-recovery-verify/{suffix}");
        refspecs.push(OsString::from(format!("{archive_ref}:{destination}")));
        destinations.insert(record.expected_oid.clone(), destination);
    }
    fetch_archive_refs(remote, &git_dir, &refspecs)?;
    for record in unique.values() {
        let destination = destinations
            .get(&record.expected_oid)
            .ok_or_else(|| format!("missing verification destination: {}", record.expected_oid))?;
        verify_local_commit(
            &git_dir,
            destination,
            &record.expected_oid,
            &record.expected_tree,
        )?;
    }
    // Make sure the caller supplied a result for every original branch.
    if archive_refs.len() != records.len() {
        return Err("archive result did not cover every source branch".to_owned());
    }
    Ok(())
}

fn fetch_archive_refs(remote: &str, git_dir: &Path, refspecs: &[OsString]) -> Result<(), String> {
    let mut filtered = vec![
        OsString::from("--git-dir"),
        git_dir.as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from("protocol.file.allow=always"),
        OsString::from("fetch"),
        OsString::from("--quiet"),
        OsString::from("--no-tags"),
        OsString::from("--filter=blob:none"),
        OsString::from("--"),
        OsString::from(remote),
    ];
    filtered.extend(refspecs.iter().cloned());
    if git_checked(None, filtered, "fetch archive refs for verification").is_err() {
        let mut plain = vec![
            OsString::from("--git-dir"),
            git_dir.as_os_str().to_owned(),
            OsString::from("-c"),
            OsString::from("protocol.file.allow=always"),
            OsString::from("fetch"),
            OsString::from("--quiet"),
            OsString::from("--no-tags"),
            OsString::from("--"),
            OsString::from(remote),
        ];
        plain.extend(refspecs.iter().cloned());
        git_checked(None, plain, "fetch archive refs for verification")?;
    }
    Ok(())
}

fn verify_local_commit(
    git_dir: &Path,
    reference: &str,
    expected_oid: &str,
    expected_tree: &str,
) -> Result<(), String> {
    let actual_oid = local_ref_oid(git_dir, reference)?
        .ok_or_else(|| format!("verification ref is missing: {reference}"))?;
    if actual_oid != expected_oid {
        return Err(format!("verification ref tip differs: {reference}"));
    }
    let commit_type = git_stdout(
        Some(git_dir),
        [
            OsStr::new("cat-file"),
            OsStr::new("-t"),
            OsStr::new(expected_oid),
        ],
        "verify archived commit object",
    )?;
    if commit_type.trim() != "commit" {
        return Err(format!("archive tip is not a commit: {expected_oid}"));
    }
    let tree = format!("{expected_oid}^{{tree}}");
    let resolved_tree = git_stdout(
        Some(git_dir),
        [OsStr::new("rev-parse"), OsStr::new(&tree)],
        "resolve archived tree",
    )?;
    if resolved_tree.trim() != expected_tree {
        return Err(format!(
            "archived tree differs from expected commit tree: {expected_oid}"
        ));
    }
    Ok(())
}

fn local_ref_oid(git_dir: &Path, reference: &str) -> Result<Option<String>, String> {
    let output = git_stdout(
        Some(git_dir),
        [
            OsStr::new("for-each-ref"),
            OsStr::new("--format=%(objectname)"),
            OsStr::new(reference),
        ],
        "read isolated archive ref",
    )?;
    let mut lines = output.lines();
    let first = lines.next().map(str::to_owned);
    if lines.next().is_some() {
        return Err(format!("isolated ref is ambiguous: {reference}"));
    }
    Ok(first)
}

fn source_stage_ref(original_ref: &str) -> String {
    let digest = Sha256::digest(original_ref.as_bytes());
    let mut suffix = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut suffix, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("refs/archive/find-and-recovery-source/{suffix}")
}

fn git_stdout<I, S>(cwd: Option<&Path>, args: I, context: &str) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_checked(cwd, args, context)?;
    String::from_utf8(output.stdout).map_err(|_| format!("{context} returned non-UTF-8 output"))
}

fn git_checked<I, S>(cwd: Option<&Path>, args: I, context: &str) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(cwd);
    command.args(args);
    let output = command
        .output()
        .map_err(|error| format!("{context}: could not start git: {error}"))?;
    if output.status.success() {
        return Ok(output);
    }
    // Fetch and push diagnostics can echo credentials embedded in a URL.
    // Keep failures actionable without returning remote stderr.
    Err(format!("{context} failed ({})", output.status))
}

fn git_command(cwd: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("--git-dir").arg(cwd);
    }
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if matches!(
            name.as_ref(),
            "GIT_DIR"
                | "GIT_WORK_TREE"
                | "GIT_COMMON_DIR"
                | "GIT_CONFIG"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_INDEX_FILE"
        ) || name.starts_with("GIT_CONFIG_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_path(path: &Path) -> &OsStr {
    path.as_os_str()
}
