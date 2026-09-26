mod conditional_delete;
mod dedupe;
mod github_ref;
#[path = "lfs_batch.rs"]
mod lfs_batch;
#[path = "remote_lfs.rs"]
mod remote_lfs;
mod remote_snapshot;
mod secret_scan;

use clap::{Parser, Subcommand};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

#[derive(Parser)]
#[command(
    name = "find-and-recovery",
    about = "Find matching Git copies, push branches to recovery refs, and clean pushed copies"
)]
struct Cli {
    #[arg(long)]
    remote: String,
    #[arg(long)]
    state: Option<PathBuf>,
    #[command(subcommand)]
    command: Phase,
}
#[derive(Subcommand)]
enum Phase {
    Scan {
        #[arg(long, action = clap::ArgAction::Append)]
        roots: Vec<PathBuf>,
        #[arg(long)]
        root_list: Option<PathBuf>,
        /// Traverse from the filesystem root; skipped and inaccessible paths remain reported.
        #[arg(long)]
        exhaustive: bool,
        /// Allow this exact canonical temporary recovery repository path through the fixture guard.
        #[arg(long = "allow-temp-repository", action = clap::ArgAction::Append)]
        allow_temp_repositories: Vec<PathBuf>,
    },
    Refresh {
        #[arg(long)]
        path: PathBuf,
    },
    Preserve {
        #[arg(long)]
        branches_only: bool,
    },
    Verify,
    Preview {
        #[arg(long)]
        branches_only: bool,
    },
    Cleanup {
        #[arg(long)]
        execute: bool,
        #[arg(long)]
        branches_only: bool,
    },
    /// Continue the same fully revalidated cleanup workflow after interruption.
    Resume {
        #[arg(long)]
        execute: bool,
        #[arg(long)]
        branches_only: bool,
    },
    /// Show partial preservation status; execution is always refused.
    ResumePartial {
        #[arg(long)]
        execute: bool,
    },
    Dedupe {
        #[arg(long)]
        execute: bool,
        /// Include every branch in the selected repository's preview scope.
        /// Execution still requires verified server protection and PR facts.
        #[arg(long)]
        all_unprotected: bool,
        /// Emit the complete machine-readable deletion plan as JSON.
        #[arg(long)]
        json: bool,
    },
    RecordRecoveredDeletion {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        ref_prefix: String,
    },
}

#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct Worktree {
    path: String,
    head: Option<String>,
    branch: Option<String>,
    detached: bool,
    bare: bool,
    missing: bool,
    foreign_registration: bool,
    status: Vec<String>,
    untracked: Vec<String>,
    ignored: Vec<String>,
    stashes: Vec<String>,
    nested_repositories: Vec<String>,
    index_tree: Option<String>,
    worktree_tree: Option<String>,
    fingerprint: String,
}
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct Branch {
    name: String,
    commit: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
struct Saved {
    source: String,
    name: String,
    commit: String,
    remote_ref: String,
    #[serde(default)]
    created_by_this_run: bool,
    #[serde(default)]
    retained_ref: Option<String>,
    tree: Option<String>,
    verification: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
struct ExistingRef {
    reference: String,
    object: String,
    object_type: String,
    verification: String,
}
/// Unindexed temporary pack/index files are opaque local data. Inventory them
/// by content and filesystem identity, then block cleanup while any exist.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
struct GarbageObjectArtifact {
    path: String,
    kind: String,
    size: u64,
    modified_ns: Option<u128>,
    device: Option<u64>,
    inode: Option<u64>,
    sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
struct TemporaryPathAuthorization {
    canonical_path: String,
    common_dir: String,
    remote: String,
    device: Option<u64>,
    inode: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Repository {
    path: String,
    common_dir: String,
    kind: String,
    #[serde(default)]
    device: Option<u64>,
    #[serde(default)]
    inode: Option<u64>,
    matched_paths: Vec<String>,
    branches: Vec<Branch>,
    refs: Vec<String>,
    worktrees: Vec<Worktree>,
    stashes: Vec<String>,
    unreachable_commits: Vec<String>,
    unreachable_noncommits: Vec<String>,
    alternates: Vec<String>,
    lfs_files: Vec<String>,
    #[serde(default)]
    lfs_preservation: String,
    #[serde(default)]
    lfs_objects: Vec<remote_lfs::LfsObject>,
    /// Unindexed tmp_pack_*/tmp_idx_* files under shared Git object storage.
    #[serde(default)]
    object_garbage: Vec<GarbageObjectArtifact>,
    #[serde(default)]
    temporary_path_authorization: Option<TemporaryPathAuthorization>,
    inventory_complete: bool,
    inventory_errors: Vec<String>,
    saved: Vec<Saved>,
    /// Local refs outside the normal branch/stash inventory that already
    /// exist remotely at the exact same object ID and were isolated-verified.
    #[serde(default)]
    existing_refs: Vec<ExistingRef>,
    /// Per-object/per-snapshot failures retained for machine-readable review.
    #[serde(default)]
    preservation_errors: Vec<String>,
    preservation: String,
    verification: String,
    verification_error: Option<String>,
    deletion: String,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct DeletionRecord {
    local_path: String,
    common_dir: String,
    local_branches: Vec<Branch>,
    snapshots: Vec<Saved>,
    #[serde(default)]
    existing_refs: Vec<ExistingRef>,
    removed_paths: Vec<String>,
    completed_unix: u64,
    #[serde(default)]
    recorded_unix: u64,
    #[serde(default)]
    reconstructed: bool,
    #[serde(default)]
    local_state_verified: bool,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    recovery_ref_prefix: String,
}
#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    schema_version: u32,
    remote: String,
    generated_unix: u64,
    roots: Vec<String>,
    coverage_gaps: Vec<String>,
    repositories: Vec<Repository>,
    /// Carry-forward ownership evidence survives a fresh discovery scan,
    /// which replaces the repository inventory and otherwise drops saved refs.
    #[serde(default)]
    recovery_ownership: Vec<Saved>,
    /// Durable local-path-to-recovery-ref mappings for copies already removed.
    #[serde(default)]
    deletion_history: Vec<DeletionRecord>,
    /// Remote-only reconstructions without proof of prior local cleanup.
    /// These must never be treated as completed local deletions.
    #[serde(default)]
    unverified_deletion_history: Vec<DeletionRecord>,
    deleted: Vec<String>,
}

fn record_deleted_copy(manifest: &mut Manifest, repository: &Repository, removed_paths: &[String]) {
    let record = DeletionRecord {
        local_path: repository.path.clone(),
        common_dir: repository.common_dir.clone(),
        local_branches: repository.branches.clone(),
        snapshots: repository.saved.clone(),
        existing_refs: repository.existing_refs.clone(),
        removed_paths: removed_paths.to_vec(),
        completed_unix: now(),
        recorded_unix: now(),
        reconstructed: false,
        local_state_verified: true,
        evidence: "cleanup-completed-after-live-inventory-and-remote-ref-checks".into(),
        recovery_ref_prefix: String::new(),
    };
    if !manifest.deletion_history.iter().any(|previous| {
        previous.local_path == record.local_path
            && previous.common_dir == record.common_dir
            && previous.snapshots == record.snapshots
            && previous.existing_refs == record.existing_refs
            && previous.removed_paths == record.removed_paths
    }) {
        manifest.deletion_history.push(record);
    }
    for path in removed_paths {
        if !manifest.deleted.contains(path) {
            manifest.deleted.push(path.clone());
        }
    }
}

fn merge_deletion_history(target: &mut Manifest, previous: &Manifest) {
    target.deleted = previous.deleted.clone();
    for record in &previous.unverified_deletion_history {
        if !target.unverified_deletion_history.iter().any(|existing| {
            existing.local_path == record.local_path
                && existing.recovery_ref_prefix == record.recovery_ref_prefix
                && existing.snapshots == record.snapshots
        }) {
            target.unverified_deletion_history.push(record.clone());
        }
        target.deleted.retain(|path| {
            path != &record.local_path
                && !record.removed_paths.iter().any(|removed| removed == path)
        });
    }
    for record in &previous.deletion_history {
        if record.reconstructed || !record.local_state_verified {
            if !target.unverified_deletion_history.iter().any(|existing| {
                existing.local_path == record.local_path
                    && existing.recovery_ref_prefix == record.recovery_ref_prefix
                    && existing.snapshots == record.snapshots
            }) {
                target.unverified_deletion_history.push(record.clone());
            }
            target.deleted.retain(|path| {
                path != &record.local_path
                    && !record.removed_paths.iter().any(|removed| removed == path)
            });
            continue;
        }
        if !target.deletion_history.iter().any(|existing| {
            existing.local_path == record.local_path
                && existing.common_dir == record.common_dir
                && existing.completed_unix == record.completed_unix
        }) {
            target.deletion_history.push(record.clone());
        }
    }
    // Upgrade manifests written before deletion_history existed. The old
    // repository row still contains the branch/snapshot mapping and path.
    for repository in &previous.repositories {
        if !repository.deletion.starts_with("deleted")
            || target.deletion_history.iter().any(|record| {
                record.local_path == repository.path
                    && record.snapshots == repository.saved
                    && record.existing_refs == repository.existing_refs
            })
        {
            continue;
        }
        let removed_paths = previous
            .deleted
            .iter()
            .filter(|path| {
                *path == &repository.path
                    || repository
                        .worktrees
                        .iter()
                        .any(|worktree| &worktree.path == *path)
            })
            .cloned()
            .collect::<Vec<_>>();
        target.deletion_history.push(DeletionRecord {
            local_path: repository.path.clone(),
            common_dir: repository.common_dir.clone(),
            local_branches: repository.branches.clone(),
            snapshots: repository.saved.clone(),
            existing_refs: repository.existing_refs.clone(),
            removed_paths: if removed_paths.is_empty() {
                vec![repository.path.clone()]
            } else {
                removed_paths
            },
            completed_unix: previous.generated_unix,
            recorded_unix: previous.generated_unix,
            reconstructed: false,
            local_state_verified: true,
            evidence: "migrated-from-previous-deleted-manifest-row".into(),
            recovery_ref_prefix: String::new(),
        });
    }
}

fn merge_recovery_ownership(target: &mut Manifest, previous: &Manifest) {
    let mut seen = target
        .recovery_ownership
        .iter()
        .map(|saved| {
            (
                saved.remote_ref.clone(),
                saved.commit.clone(),
                saved.source.clone(),
                saved.name.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    for saved in previous.recovery_ownership.iter().chain(
        previous
            .repositories
            .iter()
            .flat_map(|repo| repo.saved.iter()),
    ) {
        // Keep every saved recovery mapping across a fresh scan. Dedupe still
        // selects only branch records and checks their exact remote OID; these
        // non-branch mappings preserve the audit trail for local snapshots.
        if saved.remote_ref.starts_with("recovery/")
            && seen.insert((
                saved.remote_ref.clone(),
                saved.commit.clone(),
                saved.source.clone(),
                saved.name.clone(),
            ))
        {
            target.recovery_ownership.push(saved.clone());
        }
    }
}

fn record_recovered_deletion(
    manifest: &mut Manifest,
    path: &Path,
    ref_prefix: &str,
) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("recovered deletion path must be absolute".into());
    }
    // The recovery namespace component is a hash of Git's common directory.
    // Require prior local inventory/history to bind that namespace to this
    // exact path; an arbitrary absent path plus a valid-looking prefix is not
    // evidence that this tool deleted it.
    let path_string = path.to_string_lossy().into_owned();
    let known_common_dirs = manifest
        .repositories
        .iter()
        .filter(|repo| {
            repo.path == path_string
                || repo
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == path_string)
        })
        .map(|repo| repo.common_dir.as_str())
        .chain(manifest.deletion_history.iter().filter_map(|record| {
            (record.local_path == path_string
                || record
                    .removed_paths
                    .iter()
                    .any(|removed| removed == &path_string))
            .then_some(record.common_dir.as_str())
        }))
        .filter(|common_dir| !common_dir.is_empty())
        .collect::<BTreeSet<_>>();
    if known_common_dirs.is_empty()
        || !known_common_dirs
            .iter()
            .any(|common_dir| hash_name(common_dir).starts_with(ref_prefix))
    {
        return Err("path and recovery prefix do not match known local inventory/history".into());
    }
    if !matches!(ref_prefix.len(), 8..=64)
        || !ref_prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("recovery ref prefix must be 8 to 64 hexadecimal characters".into());
    }
    match fs::symlink_metadata(path) {
        Ok(_) => return Err(format!("recovered path still exists: {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("check recovered path {}: {error}", path.display())),
    }

    let references = remote_recovery_refs(&manifest.remote, ref_prefix)?;
    if references.is_empty() {
        return Err(format!(
            "no remote recovery refs found for prefix {ref_prefix}"
        ));
    }
    let mut snapshots = Vec::with_capacity(references.len());
    let mut local_branches = Vec::new();
    for (remote_ref, commit) in references {
        let known = manifest
            .recovery_ownership
            .iter()
            .chain(
                manifest
                    .repositories
                    .iter()
                    .flat_map(|repo| repo.saved.iter()),
            )
            .find(|saved| saved.remote_ref == remote_ref && saved.commit == commit);
        let (source, name) = if let Some(saved) = known {
            (saved.source.clone(), saved.name.clone())
        } else {
            infer_reconstructed_snapshot_name(ref_prefix, &remote_ref)
        };
        if source == "branch" {
            if let Some(saved) = known {
                let branch_name = saved
                    .name
                    .strip_prefix("branch:")
                    .unwrap_or(&saved.name)
                    .to_owned();
                local_branches.push(Branch {
                    name: branch_name,
                    commit: commit.clone(),
                });
            }
        }
        snapshots.push(Saved {
            source,
            name,
            commit,
            remote_ref,
            created_by_this_run: known.is_some_and(|saved| saved.created_by_this_run),
            retained_ref: None,
            tree: None,
            verification: "reconstructed-remote-only-not-local-verified".into(),
        });
    }
    local_branches.sort_by(|left, right| left.name.cmp(&right.name));
    local_branches.dedup_by(|left, right| left.name == right.name && left.commit == right.commit);

    let existing = manifest.deletion_history.iter().find(|record| {
        record.local_path == path_string && record.recovery_ref_prefix == ref_prefix
    });
    let existing = existing.or_else(|| {
        manifest.unverified_deletion_history.iter().find(|record| {
            record.local_path == path_string && record.recovery_ref_prefix == ref_prefix
        })
    });
    if let Some(existing) = existing {
        let same = existing.snapshots == snapshots;
        if same {
            return Ok(());
        }
        return Err("recovered deletion record exists but remote refs changed".into());
    }

    manifest.unverified_deletion_history.push(DeletionRecord {
        local_path: path_string.clone(),
        common_dir: String::new(),
        local_branches,
        snapshots,
        existing_refs: Vec::new(),
        removed_paths: vec![path_string.clone()],
        completed_unix: 0,
        recorded_unix: now(),
        reconstructed: true,
        local_state_verified: false,
        evidence: format!(
            "reconstructed-from-read-only-git-ls-remote; prefix={ref_prefix}; local repository state unavailable"
        ),
        recovery_ref_prefix: ref_prefix.to_owned(),
    });
    Ok(())
}

fn infer_reconstructed_snapshot_name(prefix: &str, remote_ref: &str) -> (String, String) {
    let identity = remote_ref
        .strip_prefix(&format!("recovery/find-and-recovery/{prefix}/"))
        .unwrap_or(remote_ref);
    if let Some(rest) = identity.strip_prefix("worktree-") {
        let path_hash = rest.split('-').next().unwrap_or("unknown");
        return (
            "worktree-snapshot".into(),
            format!("worktree:<path-unavailable:{path_hash}>"),
        );
    }
    if identity.starts_with("recovery-local-") {
        return (
            "recovery-local".into(),
            format!("recovery-local:<original-name-unavailable:{identity}>"),
        );
    }
    if identity.starts_with("branch-") {
        return (
            "branch".into(),
            format!("branch:<original-name-unavailable:{identity}>"),
        );
    }
    (
        "reconstructed-remote-ref".into(),
        format!("remote-ref:{identity}"),
    )
}

fn remote_recovery_refs(remote: &str, prefix: &str) -> Result<Vec<(String, String)>, String> {
    let full_prefix = format!("refs/heads/recovery/find-and-recovery/{prefix}/");
    let pattern = format!("{full_prefix}*");
    let args = vec![
        "git".to_owned(),
        "ls-remote".to_owned(),
        "--heads".to_owned(),
        "--".to_owned(),
        remote.to_owned(),
        pattern,
    ];
    let output = run(&args, None, &[])
        .map_err(|_| "could not start remote recovery-ref listing".to_owned())?;
    if !output.status.success() {
        return Err(format!(
            "remote recovery-ref listing failed ({})",
            output.status
        ));
    }
    let listing = String::from_utf8(output.stdout)
        .map_err(|_| "remote recovery-ref listing is not UTF-8".to_owned())?;
    let mut refs = BTreeMap::new();
    for line in listing.lines() {
        let (oid, reference) = line
            .split_once('\t')
            .ok_or_else(|| format!("malformed remote ref listing row: {line:?}"))?;
        if !reference.starts_with(&full_prefix)
            || (oid.len() != 40 && oid.len() != 64)
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("remote returned an invalid recovery ref row".into());
        }
        let relative = reference
            .strip_prefix("refs/heads/")
            .expect("validated full prefix includes refs/heads/")
            .to_owned();
        if refs.insert(relative, oid.to_owned()).is_some() {
            return Err("remote returned duplicate recovery ref names".into());
        }
    }
    Ok(refs.into_iter().collect())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn require_partial_resume_preview_only(execute: bool) -> Result<(), String> {
    if execute {
        Err(
            "resume-partial --execute is disabled: partial preservation cannot authorize cleanup"
                .into(),
        )
    } else {
        Ok(())
    }
}

fn default_state_dir() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("find-and-recovery"));
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = env::var_os("HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("find-and-recovery"));
    }
    #[cfg(target_os = "windows")]
    if let Some(app_data) = env::var_os("APPDATA").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(app_data).join("find-and-recovery"));
    }
    if let Some(home) = env::var_os("HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("find-and-recovery"));
    }
    Err("cannot choose state directory: XDG_STATE_HOME and HOME are unset".into())
}

fn default_scan_roots(exhaustive: bool) -> Vec<PathBuf> {
    if exhaustive {
        return vec![PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })];
    }
    let mut roots = Vec::new();
    if let Some(home) = env::var_os("HOME").filter(|path| !path.is_empty()) {
        roots.push(PathBuf::from(home));
    }
    #[cfg(target_os = "macos")]
    roots.extend(["/Volumes", "/tmp", "/private/tmp", "/opt", "/usr/local"].map(PathBuf::from));
    #[cfg(target_os = "linux")]
    roots.extend(["/mnt", "/media", "/tmp", "/var/tmp", "/opt", "/srv"].map(PathBuf::from));
    #[cfg(target_os = "windows")]
    if let Some(profile) = env::var_os("USERPROFILE").filter(|path| !path.is_empty()) {
        roots.push(PathBuf::from(profile));
    }
    roots
}

fn home_directory() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|path| !path.is_empty()))
        .map(PathBuf::from)
}

fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none_or(|parent| parent == path)
}

fn read_root_list(path: &Path) -> Result<Vec<PathBuf>, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("read root list {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        return Err("root list must be NUL-delimited and end with NUL".into());
    }
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            #[cfg(unix)]
            {
                Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
            }
            #[cfg(not(unix))]
            {
                let path = std::str::from_utf8(path)
                    .map_err(|error| format!("root list path is not UTF-8: {error}"))?;
                Ok(PathBuf::from(OsString::from(path)))
            }
        })
        .collect()
}
fn run(args: &[String], cwd: Option<&Path>, extra_env: &[(&str, &str)]) -> io::Result<Output> {
    let mut c = Command::new(&args[0]);
    c.args(&args[1..]);
    if let Some(p) = cwd {
        c.current_dir(p);
    }
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_INDEX_FILE",
    ] {
        c.env_remove(key);
    }
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c.output()
}
fn out(args: &[String], cwd: Option<&Path>, extra_env: &[(&str, &str)]) -> Result<String, String> {
    let mut args = args.to_vec();
    if args.first().is_some_and(|a| a == "git") {
        if let Some(i) = args.iter().position(|a| a == "-C") {
            if let Some(path) = args.get(i + 1).cloned() {
                args.splice(
                    i + 2..i + 2,
                    ["-c".into(), format!("safe.directory={path}")],
                );
            }
        }
    }
    let o = run(&args, cwd, extra_env).map_err(|e| format!("{}: {e}", args[0]))?;
    if !o.status.success() {
        return Err(format!(
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}
fn git(path: &Path, args: &[&str]) -> Result<String, String> {
    let mut a = vec![
        "git".to_string(),
        "-C".into(),
        path.to_string_lossy().into_owned(),
        "-c".into(),
        format!("safe.directory={}", path.to_string_lossy()),
    ];
    a.extend(args.iter().map(|x| x.to_string()));
    out(&a, None, &[("GIT_NO_LAZY_FETCH", "1")])
}
fn git_with_index(path: &Path, args: &[&str], index: &Path) -> Result<String, String> {
    let mut a = vec![
        "git".to_string(),
        "-C".into(),
        path.to_string_lossy().into_owned(),
        "-c".into(),
        format!("safe.directory={}", path.to_string_lossy()),
    ];
    a.extend(args.iter().map(|x| x.to_string()));
    let common_raw = git(path, &["rev-parse", "--git-common-dir"])?;
    let common_path = PathBuf::from(common_raw.trim());
    let common = if common_path.is_absolute() {
        common_path
    } else {
        path.join(common_path)
    };
    let common = fs::canonicalize(common).map_err(|e| e.to_string())?;
    let object_dir = index
        .parent()
        .ok_or("temporary index has no parent")?
        .join("objects");
    fs::create_dir_all(&object_dir).map_err(|e| e.to_string())?;
    out(
        &a,
        None,
        &[
            ("GIT_NO_LAZY_FETCH", "1"),
            (
                "GIT_INDEX_FILE",
                index.to_str().ok_or("non-UTF8 temporary index")?,
            ),
            (
                "GIT_OBJECT_DIRECTORY",
                object_dir
                    .to_str()
                    .ok_or("non-UTF8 temporary object directory")?,
            ),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                common
                    .join("objects")
                    .to_str()
                    .ok_or("non-UTF8 common object directory")?,
            ),
        ],
    )
}
fn canon_url(u: &str) -> String {
    let value = u.trim();
    if let Ok(nwo) = github_ref::repository_nwo(value) {
        return format!("https://github.com/{}", nwo.to_ascii_lowercase());
    }
    if value.starts_with("file://") || Path::new(value).is_absolute() {
        return fs::canonicalize(value)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| value.to_owned());
    }
    if let Some((scheme, rest)) = value.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, authority),
        };
        let prefix = user.map_or_else(String::new, |user| format!("{user}@"));
        return format!(
            "{}://{}{}{}",
            scheme.to_ascii_lowercase(),
            prefix,
            host.to_ascii_lowercase(),
            if path.is_empty() {
                String::new()
            } else {
                format!("/{}", path.trim_end_matches('/'))
            }
        );
    }
    value.to_owned()
}
fn target_path(path: &Path, remote: &str) -> bool {
    let mut p = path.to_path_buf();
    let mut seen = HashSet::new();
    for _ in 0..12 {
        let Ok(c) = fs::canonicalize(&p) else {
            return false;
        };
        if !seen.insert(c.clone()) {
            return false;
        }
        p = c;
        if reject_remote_url_rewrite(remote, Some(&p)).is_err() {
            return false;
        }
        let Ok(cfg) = git(&p, &["config", "--get-regexp", r"^remote\..*\.url$"]) else {
            return false;
        };
        return cfg
            .lines()
            .filter_map(|line| line.split_once(' ').map(|(_, value)| value))
            .any(|value| canon_url(value) == canon_url(remote));
    }
    false
}

fn branch_upstreams_target_remote(path: &Path, target: &str) -> Result<bool, String> {
    let origin_urls = git(path, &["config", "--get-all", "remote.origin.url"]).unwrap_or_default();
    let mut matches_target = false;
    let origin_urls = origin_urls.lines().collect::<Vec<_>>();
    if !origin_urls.is_empty() {
        if origin_urls
            .iter()
            .any(|url| canon_url(url) != canon_url(target))
        {
            return Ok(false);
        }
        matches_target = true;
    }

    let refs = git(
        path,
        &[
            "for-each-ref",
            "--format=%(refname:short)%00%(upstream:remotename)%00%(upstream:remoteref)",
            "refs/heads/",
        ],
    )?;
    let mut found_branch = false;
    for line in refs.lines() {
        let mut fields = line.split('\0');
        let (Some(_branch), Some(remote_name), Some(upstream_ref)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Ok(false);
        };
        found_branch = true;
        if remote_name.is_empty() {
            // A branch without an upstream is still unambiguous when the
            // repository's origin is the explicitly selected target.
            if matches_target {
                continue;
            }
            return Ok(false);
        }
        if !upstream_ref.starts_with("refs/heads/") {
            return Ok(false);
        }
        let key = format!("remote.{remote_name}.url");
        let urls = git(path, &["config", "--get-all", &key]).unwrap_or_default();
        if !urls.lines().any(|url| canon_url(url) == canon_url(target)) {
            return Ok(false);
        }
        matches_target = true;
    }
    Ok(matches_target && (found_branch || !origin_urls.is_empty()))
}
fn common_dir(path: &Path) -> Result<PathBuf, String> {
    let s = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(s.trim()))
}

fn is_bare_git_directory(path: &Path) -> bool {
    git(path, &["config", "--bool", "--get", "core.bare"]).is_ok_and(|value| value.trim() == "true")
}
fn worktree_path_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "cannot inspect worktree path {}: {error}",
            path.display()
        )),
    }
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("expected absolute path: {}", path.display()));
    }
    let mut normalized = PathBuf::new();
    let mut normal_depth = 0usize;
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                normalized.push(component.as_os_str())
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normal_depth == 0 {
                    return Err(format!("path escapes filesystem root: {}", path.display()));
                }
                normalized.pop();
                normal_depth -= 1;
            }
            std::path::Component::Normal(part) => {
                normalized.push(part);
                normal_depth += 1;
            }
        }
    }
    Ok(normalized)
}

fn validate_registered_gitdir(entry: &Path, linked_gitdir: &Path) -> Result<PathBuf, String> {
    if !linked_gitdir.is_absolute() || linked_gitdir.file_name().is_none_or(|name| name != ".git") {
        return Err(format!(
            "invalid linked worktree gitdir: {}",
            linked_gitdir.display()
        ));
    }
    let linked_path = linked_gitdir.parent().ok_or("bad linked worktree gitdir")?;
    if lexical_normalize(&linked_path.join(".git"))? != lexical_normalize(linked_gitdir)? {
        return Err(format!(
            "linked worktree gitdir does not match registered path: {}",
            entry.display()
        ));
    }
    Ok(linked_path.to_path_buf())
}

fn validate_registered_commondir(entry: &Path, common: &Path) -> Result<(), String> {
    let contents = fs::read_to_string(entry.join("commondir")).map_err(|error| {
        format!(
            "cannot read registered worktree commondir {}: {error}",
            entry.display()
        )
    })?;
    let value = contents.trim();
    if value.is_empty() {
        return Err(format!(
            "empty registered worktree commondir: {}",
            entry.display()
        ));
    }
    let path = PathBuf::from(value);
    let resolved = fs::canonicalize(if path.is_absolute() {
        path
    } else {
        entry.join(path)
    })
    .map_err(|error| {
        format!(
            "cannot resolve registered worktree commondir {}: {error}",
            entry.display()
        )
    })?;
    let expected = fs::canonicalize(common).map_err(|error| {
        format!(
            "cannot resolve Git common directory {}: {error}",
            common.display()
        )
    })?;
    if resolved != expected {
        return Err(format!(
            "registered worktree points to a different Git common directory: {}",
            entry.display()
        ));
    }
    Ok(())
}

fn parse_registered_worktree_head(
    owner: &Path,
    head_text: &str,
) -> Result<(Option<String>, Option<String>, bool), String> {
    match head_text.trim().strip_prefix("ref: ") {
        Some(reference) => {
            let branch = reference
                .strip_prefix("refs/heads/")
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!("registered worktree HEAD points outside local branches: {reference}")
                })?;
            git(owner, &["check-ref-format", reference])?;
            let ref_status = git_output(owner, &["show-ref", "--verify", "--quiet", reference])?;
            let oid = match ref_status.status.code() {
                Some(0) => {
                    let commit_ref = format!("{reference}^{{commit}}");
                    let value = git(owner, &["rev-parse", "--verify", &commit_ref])?;
                    Some(validate_commit_oid(owner, value.trim())?)
                }
                Some(1) => None,
                _ => {
                    return Err(format!(
                        "cannot inspect registered worktree HEAD ref {reference}: {}",
                        String::from_utf8_lossy(&ref_status.stderr).trim()
                    ));
                }
            };
            Ok((oid, Some(branch), false))
        }
        None => Ok((
            Some(validate_commit_oid(owner, head_text.trim())?),
            None,
            true,
        )),
    }
}
#[cfg(unix)]
fn filesystem_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn verify_filesystem_identity(path: &Path, expected: Option<(u64, u64)>) -> Result<(), String> {
    let expected = expected.ok_or("filesystem identity unavailable; refusing cleanup")?;
    let actual =
        filesystem_identity(path).ok_or("cannot read filesystem identity; refusing cleanup")?;
    if actual != expected {
        return Err("repository filesystem identity changed".into());
    }
    Ok(())
}
#[cfg(not(unix))]
fn filesystem_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path).ok()?;
        Some((
            metadata.volume_serial_number()? as u64,
            metadata.file_index()?,
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}
fn inventory_object_garbage(common: &Path) -> Result<Vec<GarbageObjectArtifact>, String> {
    let objects = common.join("objects");
    let entries = fs::read_dir(&objects).map_err(|error| {
        format!(
            "cannot inspect Git object directory {}: {error}",
            objects.display()
        )
    })?;
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot read Git object directory entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("tmp_pack_") && !name.starts_with("tmp_idx_") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot stat Git garbage artifact {}: {error}",
                path.display()
            )
        })?;
        let kind = if metadata.file_type().is_file() {
            "file"
        } else if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else {
            "other"
        };
        let sha256 = match kind {
            "file" => {
                let mut file = fs::File::open(&path).map_err(|error| {
                    format!(
                        "cannot read Git garbage artifact {}: {error}",
                        path.display()
                    )
                })?;
                let mut digest = Sha256::new();
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                Some(format!("{:x}", digest.finalize()))
            }
            "symlink" => Some(hash_bytes(
                fs::read_link(&path)
                    .map_err(|error| {
                        format!(
                            "cannot read Git garbage symlink {}: {error}",
                            path.display()
                        )
                    })?
                    .to_string_lossy()
                    .as_bytes(),
            )),
            _ => None,
        };
        #[cfg(unix)]
        let (device, inode) = (Some(metadata.dev()), Some(metadata.ino()));
        #[cfg(not(unix))]
        let (device, inode) = (None, None);
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        artifacts.push(GarbageObjectArtifact {
            path: path.to_string_lossy().into_owned(),
            kind: kind.into(),
            size: metadata.len(),
            modified_ns,
            device,
            inode,
            sha256,
        });
    }
    artifacts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(artifacts)
}
fn parse_worktrees(path: &Path) -> Result<Vec<Worktree>, String> {
    let text = match git(path, &["worktree", "list", "--porcelain"]) {
        Ok(text) => text,
        Err(e) if e.contains("Invalid path") && e.contains("No such file or directory") => {
            // Git refuses to list otherwise-valid worktrees when a stale
            // linked-worktree registration points below a removed parent.
            // Recover the primary tree and every admin HEAD from metadata.
            let top = PathBuf::from(git(path, &["rev-parse", "--show-toplevel"])?.trim());
            let (head, branch, detached) = current_worktree_head(&top)?;
            let mut rows = vec![Worktree {
                path: top.to_string_lossy().into_owned(),
                head,
                branch,
                detached,
                ..Default::default()
            }];
            let common = common_dir(path)?;
            let admin = common.join("worktrees");
            match fs::symlink_metadata(&admin) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "worktree admin path is not a directory: {}",
                        admin.display()
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect worktree admin path {}: {error}",
                        admin.display()
                    ));
                }
            }
            {
                for entry in fs::read_dir(&admin).map_err(|e| {
                    format!("cannot read worktree admin path {}: {e}", admin.display())
                })? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    let entry_path = entry.path();
                    let entry_metadata =
                        fs::symlink_metadata(&entry_path).map_err(|e| e.to_string())?;
                    if !entry_metadata.file_type().is_dir() {
                        return Err(format!(
                            "invalid worktree admin entry: {}",
                            entry_path.display()
                        ));
                    }
                    let worktree_gitdir =
                        fs::read_to_string(entry_path.join("gitdir")).map_err(|e| e.to_string())?;
                    let linked_gitdir = PathBuf::from(worktree_gitdir.trim());
                    let linked_path = validate_registered_gitdir(&entry_path, &linked_gitdir)?;
                    validate_registered_commondir(&entry_path, &common)?;
                    let head_text =
                        fs::read_to_string(entry_path.join("HEAD")).map_err(|e| e.to_string())?;
                    let (head, branch, detached) =
                        parse_registered_worktree_head(path, &head_text)?;
                    let missing = !worktree_path_exists(&linked_path)?;
                    rows.push(Worktree {
                        path: linked_path.to_string_lossy().into_owned(),
                        head,
                        detached,
                        branch,
                        missing,
                        ..Default::default()
                    });
                }
            }
            return Ok(rows);
        }
        Err(e) => return Err(e),
    };
    let mut rows = Vec::new();
    let mut cur = BTreeMap::<String, String>::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(p) = cur.get("worktree") {
                let branch = cur
                    .get("branch")
                    .and_then(|x| x.strip_prefix("refs/heads/").map(str::to_owned));
                let bare = cur.contains_key("bare");
                let detached = cur.contains_key("detached");
                let head = if bare {
                    None
                } else {
                    let raw = cur
                        .get("HEAD")
                        .ok_or_else(|| format!("worktree HEAD missing from Git metadata: {p}"))?;
                    normalize_worktree_head(Path::new(p), raw, branch.as_deref(), detached)?
                };
                rows.push(Worktree {
                    path: p.clone(),
                    head,
                    branch,
                    detached,
                    bare,
                    ..Default::default()
                });
            }
            cur.clear();
            continue;
        }
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        cur.insert(k.into(), v.into());
    }
    Ok(rows)
}

fn git_output(path: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = vec![
        "git".to_string(),
        "-C".into(),
        path.to_string_lossy().into_owned(),
        "-c".into(),
        format!("safe.directory={}", path.to_string_lossy()),
    ];
    command.extend(args.iter().map(|arg| (*arg).to_owned()));
    run(&command, None, &[("GIT_NO_LAZY_FETCH", "1")]).map_err(|error| error.to_string())
}

fn object_id_length(path: &Path) -> Result<usize, String> {
    match git(path, &["rev-parse", "--show-object-format"])?.trim() {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        format => Err(format!("unsupported Git object format: {format}")),
    }
}

fn validate_commit_oid(path: &Path, oid: &str) -> Result<String, String> {
    let length = object_id_length(path)?;
    if oid.len() != length
        || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        || oid.bytes().all(|byte| byte == b'0')
    {
        return Err("worktree HEAD is not a valid nonzero object ID".into());
    }
    if git(path, &["cat-file", "-t", oid])?.trim() != "commit" {
        return Err("worktree HEAD does not identify a commit".into());
    }
    Ok(oid.to_ascii_lowercase())
}

fn normalize_worktree_head(
    path: &Path,
    raw_head: &str,
    branch: Option<&str>,
    detached: bool,
) -> Result<Option<String>, String> {
    let length = object_id_length(path)?;
    if raw_head.len() != length || !raw_head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "malformed worktree HEAD in Git metadata: {}",
            path.display()
        ));
    }
    if raw_head.bytes().all(|byte| byte == b'0') {
        let Some(branch) = branch.filter(|_| !detached) else {
            return Err(format!(
                "zero worktree HEAD is not attached to a symbolic branch: {}",
                path.display()
            ));
        };
        let reference = format!("refs/heads/{branch}");
        git(path, &["check-ref-format", &reference])?;
        if git(path, &["symbolic-ref", "--quiet", "HEAD"])?.trim() != reference {
            return Err(format!(
                "zero worktree HEAD does not match its symbolic branch: {}",
                path.display()
            ));
        }
        let reference_status = git_output(path, &["show-ref", "--verify", "--quiet", &reference])?;
        match reference_status.status.code() {
            Some(1) => Ok(None),
            Some(0) => Err(format!(
                "zero worktree HEAD has an existing branch ref: {}",
                path.display()
            )),
            _ => Err(format!(
                "cannot validate unborn symbolic HEAD at {}: {}",
                path.display(),
                String::from_utf8_lossy(&reference_status.stderr).trim()
            )),
        }
    } else {
        Ok(Some(validate_commit_oid(path, raw_head)?))
    }
}

fn current_worktree_head(path: &Path) -> Result<(Option<String>, Option<String>, bool), String> {
    let symbolic = git_output(path, &["symbolic-ref", "--quiet", "HEAD"])?;
    match symbolic.status.code() {
        Some(0) => {
            let reference = String::from_utf8_lossy(&symbolic.stdout).trim().to_owned();
            let branch = reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| format!("HEAD points outside local branches: {reference}"))?;
            git(path, &["check-ref-format", &reference])?;
            let ref_status = git_output(path, &["show-ref", "--verify", "--quiet", &reference])?;
            match ref_status.status.code() {
                Some(1) => {
                    let resolved = git_output(path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
                    if resolved.status.success() {
                        return Err(
                            "symbolic HEAD ref is missing but HEAD resolves to a commit".into()
                        );
                    }
                    Ok((None, Some(branch.to_owned()), false))
                }
                Some(0) => {
                    let head = git(path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
                    Ok((
                        Some(validate_commit_oid(path, head.trim())?),
                        Some(branch.to_owned()),
                        false,
                    ))
                }
                _ => Err(format!(
                    "cannot inspect symbolic HEAD ref: {}",
                    String::from_utf8_lossy(&ref_status.stderr).trim()
                )),
            }
        }
        Some(1) => {
            let head = git(path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
            Ok((Some(validate_commit_oid(path, head.trim())?), None, true))
        }
        _ => Err(format!(
            "cannot inspect worktree HEAD: {}",
            String::from_utf8_lossy(&symbolic.stderr).trim()
        )),
    }
}
fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file_list(path: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let o = run(
        &[
            vec![
                "git".into(),
                "-C".into(),
                path.to_string_lossy().into_owned(),
                "-c".into(),
                format!("safe.directory={}", path.to_string_lossy()),
            ],
            args.iter().map(|x| x.to_string()).collect(),
        ]
        .concat(),
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )
    .map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().into());
    }
    Ok(o.stdout
        .split(|b| *b == 0)
        .filter(|x| !x.is_empty())
        .map(|x| String::from_utf8_lossy(x).to_string())
        .collect())
}
fn snapshot_fingerprint(w: &Worktree) -> String {
    hash_bytes(
        serde_json::to_string(&(
            w.head.as_deref(),
            w.branch.as_deref(),
            w.detached,
            &w.status,
            &w.untracked,
            &w.ignored,
            &w.stashes,
            &w.nested_repositories,
            &w.index_tree,
            &w.worktree_tree,
        ))
        .unwrap_or_default()
        .as_bytes(),
    )
}
fn skipped_tree_path(value: &str) -> Option<&Path> {
    value
        .strip_prefix("<nested-repository scan skipped for generated tree: ")?
        .strip_suffix('>')
        .map(Path::new)
}
fn ensure_no_nested_git(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!(
            "generated tree is missing or unreadable: {}",
            root.display()
        ));
    }
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || entry.file_type().is_dir())
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.depth() > 0 {
            let marker = entry.path().join(".git");
            if marker.exists() || marker.is_symlink() {
                return Err(format!(
                    "nested repository blocks cleanup: {}",
                    marker.display()
                ));
            }
        }
    }
    Ok(())
}
fn validate_nested_inventory(worktree: &Worktree) -> Result<(), String> {
    for item in &worktree.nested_repositories {
        if let Some(path) = skipped_tree_path(item) {
            ensure_no_nested_git(path)?;
        } else {
            return Err(format!("nested repository blocks cleanup: {item}"));
        }
    }
    Ok(())
}
fn validate_ignored_nested_inventory(worktree: &Worktree) -> Result<(), String> {
    let root = Path::new(&worktree.path);
    for relative in &worktree.ignored {
        let candidate = root.join(relative);
        let marker = candidate.join(".git");
        if marker.exists() || marker.is_symlink() {
            return Err(format!(
                "nested repository blocks cleanup: {}",
                marker.display()
            ));
        }
        if candidate.is_dir() {
            ensure_no_nested_git(&candidate)?;
        }
    }
    Ok(())
}
fn inventory_worktree(
    owner: &Path,
    common: &Path,
    wt: &mut Worktree,
    remote: &str,
) -> Result<(), String> {
    if wt.bare {
        return Ok(());
    }
    let p = PathBuf::from(&wt.path);
    if !worktree_path_exists(&p)? {
        wt.missing = true;
        return Ok(());
    }
    let actual = common_dir(&p)?;
    if actual != common {
        wt.foreign_registration = true;
        return Ok(());
    }
    if !target_path(&p, remote) {
        wt.foreign_registration = true;
        return Ok(());
    }
    let status = git(
        &p,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    wt.status = status.lines().map(str::to_owned).collect();
    wt.untracked = file_list(&p, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    wt.ignored = file_list(
        &p,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    wt.stashes = git(&p, &["stash", "list", "--format=%H %gd %gs"])
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let mut nested = Vec::new();
    if wt.ignored.is_empty() {
        let mut skipped = Vec::new();
        for e in WalkDir::new(&p)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.file_name() == ".git" {
                    return false;
                }
                let name = e.file_name().to_string_lossy();
                if e.file_type().is_dir()
                    && [
                        "target",
                        "target-review",
                        "node_modules",
                        "vendor",
                        "dist",
                        "build",
                        "cache",
                        "debug",
                        "release",
                        "incremental",
                        "deps",
                        "shots",
                        "snapshots",
                        "artifacts",
                        "coverage",
                    ]
                    .iter()
                    .any(|excluded| name.eq_ignore_ascii_case(excluded))
                {
                    skipped.push(format!(
                        "<nested-repository scan skipped for generated tree: {}>",
                        e.path().display()
                    ));
                    return false;
                }
                true
            })
        {
            match e {
                Ok(x)
                    if x.file_type().is_dir()
                        && x.path() != p
                        && (x.path().join(".git").exists()
                            || x.path().join(".git").is_symlink()) =>
                {
                    nested.push(x.path().to_string_lossy().into_owned())
                }
                Err(e) => return Err(e.to_string()),
                _ => {}
            }
        }
        nested.extend(skipped);
    } else {
        nested
            .push("<nested-repository scan skipped because ignored content blocks cleanup>".into());
    }
    wt.nested_repositories = nested;
    wt.index_tree = tree_from_index(&p, None).ok();
    let clean = wt.status.iter().all(|line| line.starts_with('#'))
        && wt.untracked.is_empty()
        && wt.ignored.is_empty();
    if clean {
        wt.worktree_tree = if let Some(head) = &wt.head {
            git(&p, &["rev-parse", &format!("{head}^{{tree}}")])
                .ok()
                .map(|tree| tree.trim().to_owned())
        } else {
            Some(canonical_empty_tree(&p)?)
        };
    } else if wt.ignored.is_empty() {
        // Dirty worktrees need a complete content tree; clean worktrees reuse HEAD.
        // Hash tracked and untracked content that preservation snapshots.
        // Cleanup compares this tree to detect edits whose status and path list stay stable.
        wt.worktree_tree = Some(write_worktree_tree(&p, wt.head.as_deref().unwrap_or(""))?);
    }
    wt.fingerprint = snapshot_fingerprint(wt);
    let _ = owner;
    Ok(())
}

fn enumerate_unreachable(
    repo: &Path,
    worktrees: &[Worktree],
    commits: &mut Vec<String>,
    noncommits: &mut Vec<String>,
) -> Result<(), String> {
    let reachable = reachable_objects(repo, worktrees)?;
    let objects = git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    )?;
    for row in objects.lines() {
        let mut fields = row.split_whitespace();
        let (Some(oid), Some(kind)) = (fields.next(), fields.next()) else {
            continue;
        };
        if reachable.contains(oid) {
            continue;
        }
        if kind == "commit" {
            commits.push(oid.into());
        } else {
            noncommits.push(oid.into());
        }
    }
    commits.sort();
    commits.dedup();
    noncommits.sort();
    noncommits.dedup();
    Ok(())
}

fn reachable_objects(repo: &Path, worktrees: &[Worktree]) -> Result<BTreeSet<String>, String> {
    let refs = git(repo, &["for-each-ref", "--format=%(refname)"])?;
    // Walk the union once. Per-ref walks repeat shared history for every
    // branch and make large multi-worktree clones impractical.
    let mut roots = refs.lines().map(str::to_owned).collect::<Vec<_>>();
    for worktree in worktrees {
        if let Some(head) = worktree.head.as_deref() {
            roots.push(head.to_owned());
        }
    }
    roots.sort();
    roots.dedup();
    let mut reachable = BTreeSet::new();
    if !roots.is_empty() {
        let mut command = vec![
            "git".to_owned(),
            "-C".into(),
            repo.to_string_lossy().into_owned(),
            "-c".into(),
            format!("safe.directory={}", repo.to_string_lossy()),
            "rev-list".into(),
            "--objects".into(),
        ];
        command.extend(roots);
        let rows = out(&command, None, &[("GIT_NO_LAZY_FETCH", "1")])?;
        reachable.extend(
            rows.lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_owned)),
        );
    }
    // Do not recursively walk index trees here. They may represent dirty
    // worktree state, which is handled by worktree preservation and blocks
    // deletion when it cannot be safely snapshotted. Branch/ref reachability
    // is enough for the unreachable-object inventory; walking every staged
    // blob here duplicates that later preservation work and can stall scans.
    Ok(reachable)
}

fn inventory_one(path: &Path, matched: Vec<String>, remote: &str) -> Repository {
    let common = common_dir(path).unwrap_or_else(|_| path.to_path_buf());
    let bare = git(path, &["rev-parse", "--is-bare-repository"])
        .map(|x| x.trim() == "true")
        .unwrap_or(false);
    let identity = filesystem_identity(path);
    let mut r = Repository {
        path: path.to_string_lossy().into_owned(),
        common_dir: common.to_string_lossy().into_owned(),
        kind: if bare { "bare" } else { "clone" }.into(),
        device: identity.map(|x| x.0),
        inode: identity.map(|x| x.1),
        matched_paths: matched,
        inventory_complete: true,
        preservation: "pending".into(),
        verification: "not-run".into(),
        deletion: "blocked".into(),
        ..Default::default()
    };
    let mut collect = || -> Result<(), String> {
        for x in git(
            path,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(objecttype)",
            ],
        )
        .map_err(|e| e)?
        .lines()
        {
            r.refs.push(x.into());
            if let Some((n, rest)) = x
                .strip_prefix("refs/heads/")
                .and_then(|v| v.split_once(' '))
            {
                let c = rest.split_whitespace().next().unwrap_or("");
                r.branches.push(Branch {
                    name: n.into(),
                    commit: c.into(),
                });
            }
        }
        r.worktrees = parse_worktrees(path)?;
        for wt in &mut r.worktrees {
            inventory_worktree(path, &common, wt, remote)?;
        }
        if bare {
            r.stashes = git(
                path,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(refname)",
                    "refs/stash",
                ],
            )
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        } else {
            r.stashes = git(path, &["stash", "list", "--format=%H %gd %gs"])
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect();
        }
        let alt = common.join("objects/info/alternates");
        if alt.exists() {
            r.alternates = fs::read_to_string(alt)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect();
        }
        r.object_garbage = inventory_object_garbage(&common)?;
        enumerate_unreachable(
            path,
            &r.worktrees,
            &mut r.unreachable_commits,
            &mut r.unreachable_noncommits,
        )?;
        let represented = reachable_objects(path, &r.worktrees)?;
        r.unreachable_noncommits
            .retain(|oid| !represented.contains(oid));
        let ref_heads = r
            .refs
            .iter()
            .filter_map(|entry| {
                let mut fields = entry.split_whitespace();
                let _reference = fields.next()?;
                let oid = fields.next()?;
                (fields.next()? == "commit").then(|| oid.to_owned())
            })
            .collect::<BTreeSet<_>>();
        let mut lfs_files = git(path, &["lfs", "ls-files", "--all", "--long"])
            .map_err(|error| format!("could not inventory LFS payloads: {error}"))?
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for head in r
            .worktrees
            .iter()
            .filter_map(|worktree| worktree.head.as_deref())
        {
            if !ref_heads.contains(head) {
                let files = git(path, &["lfs", "ls-files", "--long", head]).map_err(|error| {
                    format!("could not inventory detached LFS payloads: {error}")
                })?;
                lfs_files.extend(files.lines().map(str::to_owned));
            }
        }
        r.lfs_files = lfs_files.into_iter().collect();
        Ok(())
    };
    if let Err(e) = collect() {
        r.inventory_complete = false;
        r.inventory_errors.push(e);
    }
    if r.worktrees.iter().any(|w| w.foreign_registration) {
        r.inventory_complete = false;
        r.inventory_errors
            .push("foreign registered worktree/dependency".into());
    }
    r
}
fn branch_inventory(path: &Path) -> Result<Vec<Branch>, String> {
    Ok(git(
        path,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ],
    )?
    .lines()
    .filter_map(|line| {
        let (reference, commit) = line.split_once(' ')?;
        Some(Branch {
            name: reference.strip_prefix("refs/heads/")?.into(),
            commit: commit.into(),
        })
    })
    .collect())
}
fn discover(
    roots: &[PathBuf],
    remote: &str,
    mut checkpoint: impl FnMut(&[Repository], &[String]) -> Result<(), String>,
) -> Result<(Vec<Repository>, Vec<String>), String> {
    let mut stores: BTreeMap<String, (PathBuf, BTreeSet<String>)> = BTreeMap::new();
    let mut gaps = Vec::new();
    let mut canonical_roots = BTreeSet::new();
    for root in roots {
        if !root.exists() {
            gaps.push(format!("missing scan root {}", root.display()));
            continue;
        }
        match root.canonicalize() {
            Ok(path) => {
                canonical_roots.insert(path);
            }
            Err(error) => gaps.push(format!(
                "cannot resolve scan root {}: {error}",
                root.display()
            )),
        }
    }
    let scan_roots = canonical_roots
        .iter()
        .filter(|candidate| {
            !canonical_roots
                .iter()
                .any(|parent| parent != *candidate && candidate.starts_with(parent))
        })
        .cloned()
        .collect::<Vec<_>>();
    for root in &scan_roots {
        eprintln!("discovery: walking {}", root.display());
        let mut matched_roots = HashSet::<PathBuf>::new();
        let visited = std::cell::Cell::new(0usize);
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let count = visited.get() + 1;
                visited.set(count);
                if count % 100_000 == 0 {
                    eprintln!("discovery: {} entries under {}", count, root.display());
                }
                if e.file_name() == ".git" {
                    return false;
                }
                if !e.file_type().is_dir() {
                    return true;
                }
                let p = e.path();
                if p.ancestors()
                    .skip(1)
                    .any(|ancestor| matched_roots.contains(ancestor))
                {
                    return false;
                }
                if e.file_type().is_dir() {
                    let meta = p.join(".git");
                    let bare = p.join("HEAD").is_file()
                        && p.join("config").is_file()
                        && p.join("objects").is_dir();
                    if (meta.exists() || meta.is_symlink() || bare) && target_path(p, remote) {
                        matched_roots.insert(p.to_path_buf());
                    }
                }
                true
            })
        {
            let e = match entry {
                Ok(v) => v,
                Err(e) => {
                    gaps.push(e.to_string());
                    continue;
                }
            };
            if !e.file_type().is_dir() {
                continue;
            }
            let p = e.path();
            let meta = p.join(".git");
            let bare = e.file_type().is_dir()
                && p.join("HEAD").is_file()
                && p.join("config").is_file()
                && p.join("objects").is_dir();
            if (meta.exists() || meta.is_symlink() || bare) && target_path(p, remote) {
                let Ok(common) = common_dir(p) else {
                    gaps.push(format!("cannot read Git common dir: {}", p.display()));
                    continue;
                };
                let common_is_target_bare = common.is_dir()
                    && common.join("HEAD").is_file()
                    && common.join("config").is_file()
                    && common.join("objects").is_dir()
                    // A normal clone's `.git` directory can report itself as
                    // bare when Git is invoked from inside that directory.
                    // The repository's configured core.bare value identifies
                    // an actual bare clone without misclassifying its storage.
                    && is_bare_git_directory(&common)
                    && target_path(&common, remote);
                let owner = if common_is_target_bare {
                    common.clone()
                } else if common.file_name().is_some_and(|n| n == ".git") {
                    common.parent().unwrap_or(p).to_path_buf()
                } else {
                    p.to_path_buf()
                };
                let key = common.to_string_lossy().into_owned();
                let v = stores
                    .entry(key)
                    .or_insert_with(|| (owner, BTreeSet::new()));
                let path = p.to_string_lossy().into_owned();
                v.1.insert(path);
            }
            if meta.is_dir() {
                continue;
            }
        }
        eprintln!("discovery: finished {}", root.display());
    }
    let mut repos = Vec::new();
    let total = stores.len();
    for (index, (_, (owner, mut matches))) in stores.into_iter().enumerate() {
        if let Ok(wts) = parse_worktrees(&owner) {
            for wt in wts {
                let p = PathBuf::from(&wt.path);
                if p.exists() && target_path(&p, remote) {
                    matches.insert(wt.path);
                }
            }
        }
        eprintln!("inventory: {}/{} {}", index + 1, total, owner.display());
        repos.push(inventory_one(&owner, matches.into_iter().collect(), remote));
        checkpoint(&repos, &gaps)?;
    }
    Ok((repos, gaps))
}
fn save(m: &Manifest, state: &Path) -> Result<(), String> {
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let p = state.join("manifest.json");
    let tmp = state.join("manifest.json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(m).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(tmp, p).map_err(|e| e.to_string())
}
fn load(state: &Path) -> Result<Manifest, String> {
    let b = fs::read(state.join("manifest.json")).map_err(|e| e.to_string())?;
    serde_json::from_slice(&b).map_err(|e| e.to_string())
}
fn hash_name(s: &str) -> String {
    hash_bytes(s.as_bytes())[..16].into()
}
fn temporary_recovery_fixture(path: &Path) -> bool {
    let temp = fs::canonicalize(env::temp_dir()).unwrap_or_else(|_| env::temp_dir());
    let candidate = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !candidate.starts_with(&temp) {
        return false;
    }
    candidate
        .ancestors()
        .take_while(|ancestor| *ancestor != temp)
        .filter_map(Path::file_name)
        .any(|name| name.to_string_lossy().starts_with("recover-"))
}
fn temporary_path_authorization(r: &Repository, target_remote: &str) -> Result<(), String> {
    let authorization = r
        .temporary_path_authorization
        .as_ref()
        .ok_or("no exact temporary path authorization")?;
    let path = Path::new(&r.path);
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("canonicalize authorized path: {e}"))?;
    if canonical.to_string_lossy() != authorization.canonical_path || canonical != path {
        return Err("authorized repository path changed".into());
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("authorized repository is no longer a real directory".into());
    }
    #[cfg(unix)]
    if Some(metadata.dev()) != authorization.device || Some(metadata.ino()) != authorization.inode {
        return Err("authorized repository filesystem identity changed".into());
    }
    let common = fs::canonicalize(&r.common_dir).map_err(|e| e.to_string())?;
    if common.to_string_lossy() != authorization.common_dir {
        return Err("authorized repository Git common directory changed".into());
    }
    let origin = git(path, &["remote", "get-url", "origin"])?;
    if canon_url(origin.trim()) != authorization.remote
        || authorization.remote != canon_url(target_remote)
    {
        return Err("authorized repository origin no longer matches requested remote".into());
    }
    Ok(())
}
fn authorize_temporary_repositories(
    repositories: &mut [Repository],
    paths: &[PathBuf],
    target_remote: &str,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for path in paths {
        let canonical = fs::canonicalize(path)
            .map_err(|error| format!("cannot resolve allowed temporary repository: {error}"))?;
        if !path.is_absolute() {
            return Err(format!(
                "temporary repository opt-in must be an absolute path: {}",
                path.display()
            ));
        }
        if !seen.insert(canonical.clone()) {
            return Err(format!(
                "duplicate temporary repository opt-in: {}",
                canonical.display()
            ));
        }
        if !temporary_recovery_fixture(&canonical) {
            return Err(format!(
                "path is not a temporary recovery fixture: {}",
                canonical.display()
            ));
        }
        let metadata = fs::symlink_metadata(&canonical)
            .map_err(|error| format!("cannot inspect allowed temporary repository: {error}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!(
                "allowed temporary repository is not a real directory: {}",
                canonical.display()
            ));
        }
        let canonical_text = canonical.to_string_lossy().into_owned();
        let repository = repositories
            .iter_mut()
            .find(|repository| repository.path == canonical_text)
            .ok_or_else(|| {
                format!(
                    "allowed temporary path was not discovered as a repository: {}",
                    canonical.display()
                )
            })?;
        let common = fs::canonicalize(&repository.common_dir)
            .map_err(|error| format!("cannot resolve repository Git common directory: {error}"))?;
        let origin = git(&canonical, &["remote", "get-url", "origin"])?;
        if canon_url(origin.trim()) != canon_url(target_remote) {
            return Err(format!(
                "temporary fixture origin does not match requested remote: {}",
                canonical.display()
            ));
        }
        #[cfg(unix)]
        let (device, inode) = (Some(metadata.dev()), Some(metadata.ino()));
        #[cfg(not(unix))]
        let (device, inode) = (None, None);
        repository.temporary_path_authorization = Some(TemporaryPathAuthorization {
            canonical_path: canonical_text,
            common_dir: common.to_string_lossy().into_owned(),
            remote: canon_url(target_remote),
            device,
            inode,
        });
    }
    Ok(())
}
fn temporary_fixture_blocker(r: &Repository, target_remote: &str) -> Option<String> {
    if !temporary_recovery_fixture(Path::new(&r.path)) {
        return None;
    }
    temporary_path_authorization(r, target_remote)
        .err()
        .map(|error| format!("blocked-temporary-recovery-fixture:{error}"))
}

fn recovery_ref(repo: &Repository, source: &str, name: &str, oid: &str) -> String {
    let identity = format!("{source}:{name}");
    let prefix = source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    let label = format!("{prefix}-{}", &hash_name(&identity)[..12]);
    format!(
        "recovery/find-and-recovery/{}/{}-{}",
        hash_name(&repo.common_dir),
        label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            })
            .collect::<String>(),
        &oid[..oid.len().min(16)]
    )
}
fn sensitive_re(repo: &Path) -> Regex {
    let _ = repo;
    // Gitleaks handles generic key/value assignments. This second pass checks
    // high-confidence credential formats only; generic names like `let secret`
    // in source code are common and otherwise block clean history.
    Regex::new(r#"(?i)(-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----|\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16})\b)"#).unwrap()
}
static SECRET_SCAN_CACHE: OnceLock<Mutex<BTreeMap<String, Result<(), String>>>> = OnceLock::new();
static REMOTE_REF_CACHE: OnceLock<Mutex<BTreeMap<String, Result<String, String>>>> =
    OnceLock::new();
static REMOTE_COMMIT_CACHE: OnceLock<Mutex<BTreeMap<String, Result<BTreeSet<String>, String>>>> =
    OnceLock::new();

fn scan_commit(repo: &Path, oid: &str, remote: &str) -> Result<(), String> {
    let cache_key = format!(
        "{}\0{remote}\0{oid}",
        repo.canonicalize()
            .unwrap_or_else(|_| repo.to_path_buf())
            .display()
    );
    let cache = SECRET_SCAN_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(result) = cache.lock().map_err(|e| e.to_string())?.get(&cache_key) {
        return result.clone();
    }
    let result = scan_commits_uncached(repo, &[oid.to_owned()], remote, &[]);
    cache
        .lock()
        .map_err(|e| e.to_string())?
        .insert(cache_key, result.clone());
    result
}

fn scan_commits(repo: &Path, oids: &[String], remote: &str) -> Result<(), String> {
    let cache = SECRET_SCAN_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let repo_name = repo
        .canonicalize()
        .unwrap_or_else(|_| repo.to_path_buf())
        .display()
        .to_string();
    let (uncached, cached_failure) = {
        let cache = cache.lock().map_err(|e| e.to_string())?;
        let mut uncached = BTreeSet::new();
        let mut cached_failure = None;
        for oid in oids {
            match cache.get(&format!("{repo_name}\0{remote}\0{oid}")) {
                Some(Err(error)) => {
                    cached_failure.get_or_insert_with(|| error.clone());
                }
                Some(Ok(())) => continue,
                None => {
                    uncached.insert(oid.clone());
                    continue;
                }
            };
        }
        (uncached.into_iter().collect::<Vec<_>>(), cached_failure)
    };
    if uncached.is_empty() {
        return cached_failure.map_or(Ok(()), Err);
    }
    let result = scan_commits_uncached(repo, &uncached, remote, &[]);
    // A batch-level finding or scanner failure does not identify which tip
    // failed. Do not poison every per-tip cache entry; preserve falls back to
    // isolated scans to decide which tips may be uploaded.
    if result.is_err() {
        return result;
    }
    let mut cache = cache.lock().map_err(|e| e.to_string())?;
    for oid in uncached {
        cache.insert(format!("{repo_name}\0{remote}\0{oid}"), Ok(()));
    }
    cached_failure.map_or(Ok(()), Err)
}

fn scan_tip_set<F>(oids: &[String], mut scan: F) -> BTreeMap<String, Result<(), String>>
where
    F: FnMut(&[String]) -> Result<(), String>,
{
    let unique = oids.iter().cloned().collect::<BTreeSet<_>>();
    let unique = unique.into_iter().collect::<Vec<_>>();
    if unique.is_empty() {
        return BTreeMap::new();
    }
    if unique.len() == 1 {
        let oid = unique.into_iter().next().unwrap();
        return BTreeMap::from([(oid.clone(), scan(std::slice::from_ref(&oid)))]);
    }
    if scan(&unique).is_ok() {
        return unique.into_iter().map(|oid| (oid, Ok(()))).collect();
    }

    unique
        .iter()
        .map(|oid| {
            let result = scan(std::slice::from_ref(oid));
            (oid.clone(), result)
        })
        .collect()
}

fn scan_preserve_tips(
    repo: &Path,
    oids: &[String],
    remote: &str,
) -> BTreeMap<String, Result<(), String>> {
    scan_tip_set(oids, |tips| {
        if tips.len() == 1 {
            scan_commit(repo, &tips[0], remote)
        } else {
            scan_commits(repo, tips, remote)
        }
    })
}

fn scan_commit_delta(
    repo: &Path,
    oid: &str,
    parent: Option<&str>,
    remote: &str,
) -> Result<(), String> {
    let excluded = parent.map(str::to_owned).into_iter().collect::<Vec<_>>();
    scan_commits_uncached(repo, &[oid.to_owned()], remote, &excluded)
}

fn scan_commits_uncached(
    repo: &Path,
    oids: &[String],
    remote: &str,
    additionally_excluded: &[String],
) -> Result<(), String> {
    if oids.is_empty() {
        return Ok(());
    }
    // Run the maintained scanner against the exact commit ancestry before any
    // object upload. Never persist scanner output or print a finding.
    let remote_refs = if remote.is_empty() {
        String::new()
    } else {
        let cache = REMOTE_REF_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
        if let Some(value) = cache
            .lock()
            .map_err(|e| e.to_string())?
            .get(remote)
            .cloned()
        {
            value.map_err(|e| format!("cannot inspect remote history for secret scanning: {e}"))?
        } else {
            let value = out(
                &["git".into(), "ls-remote".into(), remote.into()],
                None,
                &[],
            )
            .map_err(|e| format!("cannot inspect remote history for secret scanning: {e}"));
            cache
                .lock()
                .map_err(|e| e.to_string())?
                .insert(remote.to_owned(), value.clone());
            value?
        }
    };
    // `ls-remote` can advertise commit IDs absent from this clone (shallow
    // histories, stale remote refs). `rev-list --not` rejects those IDs, so
    // exclude only remote commits that are present in this object database.
    let excluded_key = format!(
        "{}\0{remote}",
        repo.canonicalize()
            .unwrap_or_else(|_| repo.to_path_buf())
            .display()
    );
    let commit_cache = REMOTE_COMMIT_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut excluded = if let Some(value) = commit_cache
        .lock()
        .map_err(|e| e.to_string())?
        .get(&excluded_key)
        .cloned()
    {
        value?
    } else {
        let mut found = BTreeSet::new();
        for remote_oid in remote_refs
            .lines()
            .filter_map(|line| line.split_whitespace().next())
        {
            if git(
                repo,
                &["cat-file", "-e", &format!("{remote_oid}^{{commit}}")],
            )
            .is_ok()
            {
                found.insert(remote_oid.to_owned());
            }
        }
        commit_cache
            .lock()
            .map_err(|e| e.to_string())?
            .insert(excluded_key, Ok(found.clone()));
        found
    };
    excluded.extend(additionally_excluded.iter().cloned());
    let mut range = oids.join(" ");
    if !excluded.is_empty() {
        range.push_str(" --not ");
        range.push_str(&excluded.iter().cloned().collect::<Vec<_>>().join(" "));
    }
    let scan = vec![
        "gitleaks".into(),
        "git".into(),
        "--no-banner".into(),
        "--redact".into(),
        "--log-opts".into(),
        range,
        repo.to_string_lossy().into_owned(),
    ];
    run_secret_scan(scan, |finding| commit_finding_bytes(repo, finding))?;
    let r = sensitive_re(repo);
    let mut rev_args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    rev_args.extend(oids.iter().cloned());
    if !excluded.is_empty() {
        rev_args.push("--not".into());
        rev_args.extend(excluded.iter().map(|x| (*x).to_owned()));
    }
    let ids = {
        let mut args = vec![
            "git".to_owned(),
            "-C".into(),
            repo.to_string_lossy().into_owned(),
        ];
        args.extend(rev_args);
        out(&args, None, &[])?
    };
    let input = ids
        .lines()
        .filter_map(|x| x.split_whitespace().next())
        .collect::<Vec<_>>()
        .join("\n");
    let mut child = Command::new("git")
        .args([
            "-C",
            repo.to_string_lossy().as_ref(),
            "-c",
            &format!("safe.directory={}", repo.to_string_lossy()),
            "cat-file",
            "--batch",
        ])
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut stdin = child.stdin.take().ok_or("cat-file stdin unavailable")?;
    let input_writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let stdout = child.stdout.take().ok_or("cat-file stdout unavailable")?;
    let mut reader = BufReader::new(stdout);
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let h = header.trim_end();
        let fields: Vec<_> = h.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(format!("missing Git object {h}"));
        }
        let size: usize = fields[2].parse().map_err(|_| "bad object length")?;
        let mut object = vec![0; size];
        reader
            .read_exact(&mut object)
            .map_err(|_| "truncated object")?;
        if r.is_match(&object) {
            return Err(format!("secret pattern in object {}", fields[0]));
        }
        let mut terminator = [0];
        reader
            .read_exact(&mut terminator)
            .map_err(|_| "missing object terminator")?;
        if terminator[0] != b'\n' {
            return Err("malformed object terminator".into());
        }
    }
    input_writer
        .join()
        .map_err(|_| "cat-file input writer panicked")?
        .map_err(|e| format!("cannot write cat-file input: {e}"))?;
    if !child.wait().map_err(|e| e.to_string())?.success() {
        return Err("object scan incomplete".into());
    }
    Ok(())
}

fn secret_finding_exceptions() -> Result<Vec<secret_scan::FindingException>, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Baseline {
        version: u32,
        exceptions: Vec<secret_scan::FindingException>,
    }
    let baseline: Baseline =
        serde_json::from_str(include_str!("../secret-finding-exceptions.json")).map_err(|_| {
            "secret finding exception baseline is invalid; upload blocked".to_owned()
        })?;
    if baseline.version != 1 {
        return Err(
            "secret finding exception baseline version is unsupported; upload blocked".into(),
        );
    }
    Ok(baseline.exceptions)
}

fn run_secret_scan<F>(mut args: Vec<String>, resolve_blob: F) -> Result<(), String>
where
    F: FnMut(&secret_scan::Finding) -> Result<Vec<u8>, String>,
{
    let report_dir = tempfile::tempdir()
        .map_err(|_| "cannot create private scanner report directory".to_owned())?;
    let report = report_dir.path().join("redacted-report.json");
    args.extend([
        "--report-format".into(),
        "json".into(),
        "--report-path".into(),
        report.to_string_lossy().into_owned(),
    ]);
    let output = run(&args, None, &[])
        .map_err(|_| "secret scanner could not run; upload blocked".to_owned())?;
    let bytes = fs::read(&report).unwrap_or_default();
    let exceptions = secret_finding_exceptions()?;
    secret_scan::evaluate_report(output.status.code(), &bytes, &exceptions, resolve_blob)
}

fn commit_finding_bytes(repo: &Path, finding: &secret_scan::Finding) -> Result<Vec<u8>, String> {
    let commit_valid = (finding.commit.len() == 40 || finding.commit.len() == 64)
        && finding.commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    let path = Path::new(&finding.file);
    if !commit_valid
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err("secret scanner reported an invalid Git location; upload blocked".into());
    }
    let spec = format!("{}:{}", finding.commit, finding.file);
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "-c".into(),
        format!("safe.directory={}", repo.to_string_lossy()),
        "cat-file".into(),
        "blob".into(),
        spec,
    ];
    let output = run(&args, None, &[("GIT_NO_LAZY_FETCH", "1")])
        .map_err(|_| "cannot read secret scanner Git blob; upload blocked".to_owned())?;
    if !output.status.success() {
        return Err("cannot read secret scanner Git blob; upload blocked".into());
    }
    Ok(output.stdout)
}

fn worktree_finding_bytes(root: &Path, finding: &secret_scan::Finding) -> Result<Vec<u8>, String> {
    let root = root
        .canonicalize()
        .map_err(|_| "cannot resolve scanned worktree; upload blocked".to_owned())?;
    let reported = Path::new(&finding.file);
    let candidate = if reported.is_absolute() {
        reported.to_path_buf()
    } else {
        root.join(reported)
    };
    let resolved = candidate
        .canonicalize()
        .map_err(|_| "cannot resolve secret scanner finding; upload blocked".to_owned())?;
    if !resolved.starts_with(&root) || !resolved.is_file() {
        return Err("secret scanner finding escaped the scanned worktree; upload blocked".into());
    }
    fs::read(resolved).map_err(|_| "cannot read secret scanner finding; upload blocked".into())
}

/// Scan every LFS payload referenced by the repository's refs before allowing
/// Git's enabled LFS pre-push hook to upload branch objects. Gzip payloads are
/// expanded to a private temporary directory so their contents are inspected.
fn scan_lfs_payloads(repo: &Path, records: &[String]) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }
    let env = out(
        &[
            "git".into(),
            "-C".into(),
            repo.to_string_lossy().into_owned(),
            "lfs".into(),
            "env".into(),
        ],
        None,
        &[],
    )?;
    let media = env
        .lines()
        .find_map(|line| line.strip_prefix("LocalMediaDir="))
        .map(PathBuf::from)
        .ok_or("Git LFS did not report LocalMediaDir")?;
    let temporary = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut seen = BTreeSet::new();
    for record in records {
        let Some((oid, rest)) = record.split_once(' ') else {
            return Err("malformed Git LFS inventory entry".into());
        };
        if oid.len() != 64 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("unsupported Git LFS object ID".into());
        }
        if !seen.insert(oid.to_owned()) {
            continue;
        }
        let payload = media.join(&oid[..2]).join(&oid[2..4]).join(oid);
        if !payload.is_file() {
            // No local payload means this step cannot upload it. The normal
            // pre-push hook remains enabled and will fail the branch push if
            // the server needs bytes that are unavailable here.
            continue;
        }
        let filename = rest
            .trim_start_matches(['*', '-', ' '])
            .trim()
            .rsplit('/')
            .next()
            .unwrap_or("payload");
        let bytes = fs::read(&payload).map_err(|e| e.to_string())?;
        let scan_path = if bytes.starts_with(&[0x1f, 0x8b]) {
            let expanded = temporary
                .path()
                .join(format!("{oid}-{}", filename.trim_end_matches(".gz")));
            let status = Command::new("gzip")
                .args(["-dc", payload.to_string_lossy().as_ref()])
                .output()
                .map_err(|e| format!("cannot decompress LFS payload: {e}"))?;
            if !status.status.success() {
                return Err(format!("cannot decompress Git LFS payload: {oid}"));
            }
            fs::write(&expanded, status.stdout).map_err(|e| e.to_string())?;
            expanded
        } else {
            payload
        };
        let scan = vec![
            "gitleaks".into(),
            "dir".into(),
            "--redact".into(),
            "--no-banner".into(),
            scan_path.to_string_lossy().into_owned(),
        ];
        if out(&scan, None, &[]).is_err() {
            return Err(format!("secret scan blocked Git LFS payload: {oid}"));
        }
    }
    Ok(())
}

fn preserve_saved_lfs(
    repo: &Path,
    remote: &str,
    saved: &[Saved],
    inventory: &[String],
) -> Result<Vec<remote_lfs::LfsObject>, String> {
    let commits = saved
        .iter()
        .map(|entry| entry.commit.clone())
        .collect::<Vec<_>>();
    let objects = remote_lfs::inventory_local_lfs_commits(repo, &commits)?;
    let object_ids = objects
        .iter()
        .map(|object| object.oid.as_str())
        .collect::<BTreeSet<_>>();
    for record in inventory {
        let oid = record
            .split_whitespace()
            .next()
            .ok_or("malformed Git LFS inventory entry")?;
        if oid.len() != 64 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("unsupported Git LFS object ID".into());
        }
        if !object_ids.contains(oid) {
            return Err(format!(
                "saved refs do not represent inventoried Git LFS pointer {oid}"
            ));
        }
    }
    remote_lfs::validate_local_lfs_payloads(repo, &objects)?;
    if !objects.is_empty() {
        // Scan only after proving that every referenced payload is present and
        // matches its pointer. In particular, newly-created snapshots are
        // included here before their LFS bytes are uploaded.
        let records = objects
            .iter()
            .map(|object| format!("{} payload", object.oid))
            .collect::<Vec<_>>();
        scan_lfs_payloads(repo, &records)?;
        let payloads = objects
            .iter()
            .map(|object| lfs_batch::LfsPayload {
                oid: object.oid.clone(),
                size: object.size,
            })
            .collect::<Vec<_>>();
        lfs_batch::push_lfs_payloads(repo, remote, &payloads)?;
    }
    Ok(objects)
}

fn remote_oid(remote: &str, reference: &str) -> Result<Option<String>, String> {
    let a = vec![
        "git".into(),
        "ls-remote".into(),
        remote.into(),
        reference.into(),
    ];
    let s = out(&a, None, &[])?;
    Ok(s.lines()
        .next()
        .and_then(|x| x.split_whitespace().next())
        .map(str::to_owned))
}

const REMOTE_REF_QUERY_BATCH_SIZE: usize = 128;

fn remote_oids_for_refs(
    remote: &str,
    references: &[String],
) -> Result<BTreeMap<String, String>, String> {
    remote_oids_for_refs_with(references, |batch| {
        let mut args = vec![
            "git".into(),
            "ls-remote".into(),
            "--refs".into(),
            "--".into(),
            remote.into(),
        ];
        args.extend(batch.iter().cloned());
        out(&args, None, &[])
    })
}

fn remote_oids_for_refs_with<F>(
    references: &[String],
    mut query: F,
) -> Result<BTreeMap<String, String>, String>
where
    F: FnMut(&[String]) -> Result<String, String>,
{
    let mut observed = BTreeMap::new();
    for batch in references.chunks(REMOTE_REF_QUERY_BATCH_SIZE) {
        let output = query(batch)?;
        for line in output.lines() {
            let (oid, reference) = line
                .split_once('\t')
                .ok_or("malformed batched Git ls-remote output")?;
            if !batch.iter().any(|requested| requested == reference) {
                continue;
            }
            if let Some(previous) = observed.insert(reference.to_owned(), oid.to_owned()) {
                if previous != oid {
                    return Err(format!("remote returned conflicting OIDs for {reference}"));
                }
            }
        }
    }
    Ok(observed)
}

fn remote_has_branch_tip(remote: &str, commit: &str) -> Result<bool, String> {
    let listing = list_remote_branch_tips(remote)?;
    Ok(listing.contains(commit))
}
fn list_remote_branch_tips(remote: &str) -> Result<BTreeSet<String>, String> {
    let listing = out(
        &[
            "git".into(),
            "ls-remote".into(),
            "--heads".into(),
            remote.into(),
        ],
        None,
        &[],
    )?;
    Ok(listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}
fn saved_commit_is_preserved(remote: &str, saved: &Saved) -> Result<bool, String> {
    let reference = format!("refs/heads/{}", saved.remote_ref);
    Ok(remote_oid(remote, &reference)?.as_deref() == Some(saved.commit.as_str()))
}
fn reject_remote_url_rewrite(remote: &str, cwd: Option<&Path>) -> Result<(), String> {
    let args = vec![
        "git".into(),
        "config".into(),
        "--null".into(),
        "--get-regexp".into(),
        r"^url\..*\.(insteadof|pushinsteadof)$".into(),
    ];
    let output = run(&args, cwd, &[]).map_err(|e| format!("git config: {e}"))?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(());
        }
        return Err(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    for entry in output.stdout.split(|byte| *byte == 0) {
        let Some(split) = entry.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let value = String::from_utf8_lossy(&entry[split + 1..]);
        if !value.is_empty() && remote.starts_with(value.as_ref()) {
            return Err(format!(
                "remote URL rewrite could redirect configured remote; refusing: {remote}"
            ));
        }
    }
    Ok(())
}
fn push_ref(remote: &str, repo: &Path, oid: &str, reference: &str) -> Result<bool, String> {
    push_ref_with_create(remote, repo, oid, reference, |remote, reference, base| {
        create_remote_ref(remote, reference, base)
    })
}

fn create_remote_ref(remote: &str, reference: &str, base: &str) -> Result<(), String> {
    // A local bare repository can provide the same create-only precondition as
    // the GitHub API. Keep this available in production too: local remotes are
    // supported targets, and falling through to the API makes them fail closed.
    if Path::new(remote).is_dir() {
        let zeros = "0".repeat(base.len());
        let args = vec![
            "git".into(),
            "--git-dir".into(),
            remote.into(),
            "update-ref".into(),
            reference.into(),
            base.into(),
            zeros,
        ];
        out(&args, None, &[])
            .map_err(|error| format!("compare-and-create local remote ref failed: {error}"))?;
        return Ok(());
    }
    github_ref::create_ref(remote, reference, base)
}

fn push_ref_with_create<F>(
    remote: &str,
    repo: &Path,
    oid: &str,
    reference: &str,
    mut create: F,
) -> Result<bool, String>
where
    F: FnMut(&str, &str, &str) -> Result<(), String>,
{
    reject_remote_url_rewrite(remote, Some(repo))?;
    match remote_oid(remote, reference)? {
        Some(existing) if existing == oid => return Ok(false),
        Some(_) => {
            return Err(format!(
                "remote ref collision; refusing overwrite: {reference}"
            ));
        }
        None => {}
    }
    let base = match fetched_remote_ancestor(remote, repo, oid) {
        Ok(base) => base,
        Err(error)
            if error == "no fetched remote commit is a proven ancestor of the local tip"
                && commit_has_no_parent(repo, oid)? =>
        {
            // A parentless recovery snapshot (for example, work from an unborn
            // branch) has no commit ancestor to pre-create. An ordinary push
            // creates an absent ref with the receive-pack zero-OID lease; it
            // cannot overwrite a competing ref and is deliberately non-force.
            return push_ref_create_only(remote, repo, oid, reference);
        }
        Err(error) => return Err(error),
    };
    // The API is create-only. Any error (including auth, unsupported host, or
    // name collision) blocks this save; never fall back to a Git ref update.
    create(remote, reference, &base)?;
    if remote_oid(remote, reference)?.as_deref() != Some(base.as_str()) {
        return Err(format!(
            "create-ref did not leave the expected ancestor at {reference}"
        ));
    }
    push_ref_fast_forward(remote, repo, oid, reference, &base)
}

fn commit_has_no_parent(repo: &Path, oid: &str) -> Result<bool, String> {
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "rev-list".into(),
        "--parents".into(),
        "-n".into(),
        "1".into(),
        oid.into(),
    ];
    let line = out(&args, None, &[])?;
    let mut fields = line.split_whitespace();
    Ok(fields.next() == Some(oid) && fields.next().is_none())
}

fn push_ref_create_only(
    remote: &str,
    repo: &Path,
    oid: &str,
    reference: &str,
) -> Result<bool, String> {
    if remote_oid(remote, reference)?.is_some() {
        return Err(format!(
            "remote ref collision; refusing overwrite: {reference}"
        ));
    }
    git(Path::new(repo), &["check-ref-format", reference])
        .map_err(|_| format!("invalid recovery destination ref: {reference}"))?;
    let args = vec![
        "git".into(),
        "-c".into(),
        "http.version=HTTP/1.1".into(),
        "-c".into(),
        "push.followTags=false".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "push".into(),
        "--porcelain".into(),
        "--no-follow-tags".into(),
        "--no-verify".into(),
        remote.into(),
        format!("{oid}:{reference}"),
    ];
    out(&args, None, &[("GIT_LFS_SKIP_PUSH", "1")])?;
    if remote_oid(remote, reference)?.as_deref() != Some(oid) {
        return Err(format!(
            "remote ref did not reach expected parentless commit: {reference}"
        ));
    }
    Ok(true)
}

/// Fetch remote heads into an isolated object store, add the local tip, and
/// choose the nearest fetched remote commit that is an ancestor of that tip.
fn fetched_remote_ancestor(remote: &str, repo: &Path, tip: &str) -> Result<String, String> {
    let temp = tempfile::Builder::new()
        .prefix("find-recovery-common-ancestor-")
        .tempdir()
        .map_err(|error| format!("create isolated ancestry store: {error}"))?;
    let git_dir = temp.path().join("objects.git");
    let init = vec![
        "git".into(),
        "init".into(),
        "--bare".into(),
        "--quiet".into(),
        git_dir.to_string_lossy().into_owned(),
    ];
    out(&init, None, &[])?;
    let fetch_remote = vec![
        "git".into(),
        "--git-dir".into(),
        git_dir.to_string_lossy().into_owned(),
        "fetch".into(),
        "--no-tags".into(),
        "--".into(),
        remote.into(),
        "+refs/heads/*:refs/remotes/find-recovery/*".into(),
    ];
    out(&fetch_remote, None, &[])
        .map_err(|error| format!("fetch remote ancestry candidates: {error}"))?;
    let fetch_local = vec![
        "git".into(),
        "--git-dir".into(),
        git_dir.to_string_lossy().into_owned(),
        "fetch".into(),
        "--no-tags".into(),
        "--".into(),
        repo.to_string_lossy().into_owned(),
        tip.into(),
    ];
    out(&fetch_local, None, &[])
        .map_err(|error| format!("load local tip into isolated ancestry store: {error}"))?;

    let refs = vec![
        "git".into(),
        "--git-dir".into(),
        git_dir.to_string_lossy().into_owned(),
        "for-each-ref".into(),
        "--format=%(objectname)".into(),
        "refs/remotes/find-recovery".into(),
    ];
    let output = out(&refs, None, &[])?;
    let mut candidates = Vec::new();
    for remote_tip in output.lines().filter(|line| !line.is_empty()) {
        let args = vec![
            "git".into(),
            "--git-dir".into(),
            git_dir.to_string_lossy().into_owned(),
            "merge-base".into(),
            tip.into(),
            remote_tip.into(),
        ];
        let base = out(&args, None, &[])
            .ok()
            .map(|value| value.trim().to_owned());
        if let Some(base) = base.filter(|value| !value.is_empty()) {
            let count = vec![
                "git".into(),
                "--git-dir".into(),
                git_dir.to_string_lossy().into_owned(),
                "rev-list".into(),
                "--count".into(),
                format!("{base}..{tip}"),
            ];
            let distance = out(&count, None, &[])?
                .trim()
                .parse::<usize>()
                .map_err(|error| format!("invalid local ancestry distance for {base}: {error}"))?;
            candidates.push((distance, base));
        }
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .map(|(_, oid)| oid)
        .ok_or_else(|| "no fetched remote commit is a proven ancestor of the local tip".into())
}

/// Advance only the API-created ref using ordinary Git fast-forward rules.
fn push_ref_fast_forward(
    remote: &str,
    repo: &Path,
    oid: &str,
    reference: &str,
    expected_base: &str,
) -> Result<bool, String> {
    if remote_oid(remote, reference)?.as_deref() != Some(expected_base) {
        return Err(format!(
            "create-ref destination changed before fast-forward push: {reference}"
        ));
    }
    let a = vec![
        "git".into(),
        "-c".into(),
        "http.version=HTTP/1.1".into(),
        "-c".into(),
        "push.followTags=false".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "push".into(),
        "--porcelain".into(),
        "--no-follow-tags".into(),
        "--no-verify".into(),
        remote.into(),
        format!("{oid}:{reference}"),
    ];
    let output = out(&a, None, &[("GIT_LFS_SKIP_PUSH", "1")])?;
    let created = output.lines().any(|line| {
        line.starts_with("*")
            && line
                .split_whitespace()
                .any(|field| field == reference || field.ends_with(&format!(":{reference}")))
    });
    if remote_oid(remote, reference)?.as_deref() != Some(oid) {
        return Err(format!(
            "remote ref did not reach expected commit after push: {reference}"
        ));
    }
    Ok(created)
}

fn record_preservation_failure(
    repository: &mut Repository,
    failure: &mut Option<String>,
    context: String,
    error: String,
) {
    let row = format!("{context}: {error}");
    if failure.is_none() {
        *failure = Some(row.clone());
    }
    repository.preservation_errors.push(row);
}

fn preserve(m: &mut Manifest) -> Result<(), String> {
    let target_remote = m.remote.clone();
    for r in &mut m.repositories {
        if r.deletion == "deleted" && !Path::new(&r.path).exists() {
            continue;
        }
        let prior_saved = r.saved.clone();
        r.preservation = "blocked".into();
        r.saved.clear();
        r.preservation_errors.clear();
        r.existing_refs.clear();
        r.lfs_objects.clear();
        r.lfs_preservation = "pending".into();
        r.verification_error = None;
        if let Some(blocker) = temporary_fixture_blocker(r, &target_remote) {
            r.verification_error = Some(blocker);
            continue;
        }
        let inventory_only_foreign_worktrees = !r.inventory_errors.is_empty()
            && r.inventory_errors
                .iter()
                .all(|error| error == "foreign registered worktree/dependency");
        if !r.inventory_complete && !inventory_only_foreign_worktrees {
            r.verification_error = Some("incomplete Git inventory".into());
            continue;
        }
        let path = PathBuf::from(&r.path);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                r.verification_error = Some(format!(
                    "candidate path is no longer a real directory: {}",
                    path.display()
                ));
                continue;
            }
            Err(error) => {
                r.verification_error = Some(format!(
                    "candidate path unavailable during preservation: {}: {error}",
                    path.display()
                ));
                continue;
            }
        }
        let remote = m.remote.clone();
        let mut blocker = None;
        let unsupported = unsupported_local_refs(r);
        let requested = unsupported
            .iter()
            .map(|(reference, _, _)| reference.clone())
            .collect::<Vec<_>>();
        let remote_refs = match remote_oids_for_refs(&remote, &requested) {
            Ok(refs) => refs,
            Err(error) => {
                blocker
                    .get_or_insert_with(|| format!("cannot check unsupported local refs: {error}"));
                BTreeMap::new()
            }
        };
        for (reference, object, object_type) in unsupported {
            match remote_refs.get(&reference) {
                Some(remote_object) if remote_object == &object => {
                    r.existing_refs.push(ExistingRef {
                        reference,
                        object,
                        object_type,
                        verification: "pending-isolated-verification".into(),
                    });
                }
                _ => {
                    blocker.get_or_insert_with(|| {
                        format!("unsupported local ref is missing or differs remotely: {reference}")
                    });
                }
            }
        }
        match branch_upstreams_target_remote(&path, &remote) {
            Ok(true) => {}
            Ok(false) => {
                r.verification_error = Some(
                    "ambiguous matching remote: origin and local branch upstreams do not all map to the requested target".into(),
                );
                continue;
            }
            Err(error) => {
                r.verification_error = Some(format!(
                    "cannot confirm branch push remote mapping: {error}"
                ));
                continue;
            }
        }
        r.lfs_preservation = "pending-saved-ref-inventory".into();
        let current_branches = match branch_inventory(&path) {
            Ok(branches) => branches,
            Err(error) => {
                r.verification_error = Some(format!(
                    "candidate unavailable during preservation preflight: {error}"
                ));
                continue;
            }
        };
        let current_worktrees = match parse_worktrees(&path) {
            Ok(worktrees) => worktrees,
            Err(error) => {
                r.verification_error = Some(format!(
                    "candidate unavailable during preservation preflight: {error}"
                ));
                continue;
            }
        };
        let heads = |items: &[Worktree]| {
            items
                .iter()
                .map(|w| {
                    (
                        w.path.clone(),
                        w.head.clone(),
                        w.branch.clone(),
                        w.detached,
                        w.bare,
                    )
                })
                .collect::<BTreeSet<_>>()
        };
        if current_branches != r.branches || heads(&current_worktrees) != heads(&r.worktrees) {
            r.verification_error =
                Some("local branches or worktree HEADs changed since scan".into());
            continue;
        }
        let mut scan_oids = BTreeSet::new();
        scan_oids.extend(r.branches.iter().map(|branch| branch.commit.clone()));
        scan_oids.extend(r.unreachable_commits.iter().cloned());
        scan_oids.extend(
            r.stashes
                .iter()
                .filter_map(|stash| stash.split_whitespace().next().map(str::to_owned)),
        );
        scan_oids.extend(r.refs.iter().filter_map(|entry| {
            let (name, tail) = entry.split_once(' ')?;
            name.starts_with("refs/recovery-local/")
                .then(|| tail.split_whitespace().next().map(str::to_owned))
                .flatten()
        }));
        scan_oids.extend(
            r.worktrees.iter().filter_map(|worktree| {
                (worktree.detached).then(|| worktree.head.clone()).flatten()
            }),
        );
        // Scan the union once. If the union fails, inspect tips independently
        // so a secret or corrupt tip blocks only its own upload attempts.
        let mut failure = None;
        let scan_results =
            scan_preserve_tips(&path, &scan_oids.into_iter().collect::<Vec<_>>(), &remote);
        for (oid, result) in scan_results {
            if let Err(error) = result {
                record_preservation_failure(r, &mut failure, format!("scan {oid}"), error);
            }
        }
        let common = r.common_dir.clone();
        let branches = r.branches.clone();
        let worktrees = r.worktrees.clone();
        // Stash refs are worktree-private in linked checkouts. Collect each
        // registered worktree's stash list, deduplicating by commit OID.
        let mut stashes = r.stashes.clone();
        let mut stash_oids = stashes
            .iter()
            .filter_map(|stash| stash.split_whitespace().next())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for worktree in &worktrees {
            for stash in &worktree.stashes {
                if let Some(oid) = stash.split_whitespace().next() {
                    if stash_oids.insert(oid.to_owned()) {
                        stashes.push(stash.clone());
                    }
                }
            }
        }
        let unreachable = r.unreachable_commits.clone();
        let mut mapped = BTreeSet::new();
        r.preservation = "complete".into();
        for branch in branches {
            if let Err(error) = save_object(
                &remote,
                &common,
                r,
                &path,
                &format!("branch:{}", branch.name),
                &branch.commit,
                "branch",
            ) {
                record_preservation_failure(
                    r,
                    &mut failure,
                    format!("save branch {} {}", branch.name, branch.commit),
                    error,
                );
                continue;
            }
            mapped.insert(branch.commit);
        }
        {
            for entry in r.refs.clone() {
                let Some((name, tail)) = entry.split_once(' ') else {
                    continue;
                };
                if !name.starts_with("refs/recovery-local/") {
                    continue;
                }
                let oid = tail.split_whitespace().next().unwrap_or("");
                if mapped.contains(oid) {
                    continue;
                }
                if let Err(error) = save_object(
                    &remote,
                    &common,
                    r,
                    &path,
                    &format!("recovery-local:{name}"),
                    oid,
                    "recovery-local",
                ) {
                    record_preservation_failure(
                        r,
                        &mut failure,
                        format!("save recovery-local {name} {oid}"),
                        error,
                    );
                    continue;
                }
                mapped.insert(oid.to_owned());
            }
        }
        // A cached remote-tracking ref can point at work absent from every
        // current remote branch (for example, a branch deleted remotely).
        // Preserve those tips unless another saved ref already covers them.
        {
            let remote_tracking = r
                .refs
                .iter()
                .filter_map(|entry| {
                    let (name, tail) = entry.split_once(' ')?;
                    if !name.starts_with("refs/remotes/") {
                        return None;
                    }
                    Some((name.to_owned(), tail.split_whitespace().next()?.to_owned()))
                })
                .collect::<Vec<_>>();
            let remote_branch_tips = if remote_tracking.iter().any(|(_, oid)| !mapped.contains(oid))
            {
                Some(list_remote_branch_tips(&remote))
            } else {
                None
            };
            for (name, oid) in remote_tracking {
                if mapped.contains(&oid) {
                    continue;
                }
                let remote_branch_tips = match &remote_branch_tips {
                    Some(Ok(tips)) => tips,
                    Some(Err(error)) => {
                        record_preservation_failure(
                            r,
                            &mut failure,
                            format!("check remote-tracking {name} {oid}"),
                            error.clone(),
                        );
                        continue;
                    }
                    None => continue,
                };
                if remote_branch_tips.contains(&oid) {
                    continue;
                }
                if let Err(error) = save_object(
                    &remote,
                    &common,
                    r,
                    &path,
                    &format!("remote-tracking:{name}"),
                    &oid,
                    "remote-tracking",
                ) {
                    record_preservation_failure(
                        r,
                        &mut failure,
                        format!("save remote-tracking {name} {oid}"),
                        error,
                    );
                    continue;
                }
                mapped.insert(oid.to_owned());
            }
        }
        {
            for (index, stash) in stashes.iter().enumerate() {
                let Some(oid) = stash.split_whitespace().next() else {
                    record_preservation_failure(
                        r,
                        &mut failure,
                        format!("save stash index {index}"),
                        "malformed stash inventory".into(),
                    );
                    continue;
                };
                if let Err(error) = save_object(
                    &remote,
                    &common,
                    r,
                    &path,
                    &format!("stash:{index}"),
                    oid,
                    "stash",
                ) {
                    record_preservation_failure(
                        r,
                        &mut failure,
                        format!("save stash index {index} {oid}"),
                        error,
                    );
                    continue;
                }
            }
        }
        {
            for oid in unreachable {
                if let Err(error) = save_object(
                    &remote,
                    &common,
                    r,
                    &path,
                    &format!("unreachable:{oid}"),
                    &oid,
                    "unreachable",
                ) {
                    record_preservation_failure(
                        r,
                        &mut failure,
                        format!("save unreachable {oid}"),
                        error,
                    );
                    continue;
                }
                mapped.insert(oid);
            }
        }
        {
            for worktree in worktrees {
                if worktree.bare || worktree.foreign_registration {
                    continue;
                }
                if !worktree.ignored.is_empty() {
                    blocker.get_or_insert(format!(
                        "worktree has ignored content; snapshot blocked: {}",
                        worktree.path
                    ));
                    continue;
                }
                if let Err(error) = validate_nested_inventory(&worktree) {
                    blocker.get_or_insert(error);
                    continue;
                }
                if worktree.detached {
                    let Some(head) = worktree.head.as_deref() else {
                        blocker.get_or_insert(format!(
                            "detached worktree has no HEAD: {}",
                            worktree.path
                        ));
                        continue;
                    };
                    if !mapped.contains(head) {
                        if let Err(error) = save_object(
                            &remote,
                            &common,
                            r,
                            &path,
                            &format!("detached:{}", worktree.path),
                            head,
                            "detached",
                        ) {
                            record_preservation_failure(
                                r,
                                &mut failure,
                                format!("save detached worktree {} {head}", worktree.path),
                                error,
                            );
                            continue;
                        }
                        mapped.insert(head.to_owned());
                    }
                }
                if !worktree.missing {
                    if let Err(error) = save_worktree(&remote, &common, r, &worktree) {
                        record_preservation_failure(
                            r,
                            &mut failure,
                            format!("save worktree snapshot {}", worktree.path),
                            error,
                        );
                        continue;
                    }
                }
            }
        }
        if !r.unreachable_noncommits.is_empty() {
            let mut represented = BTreeSet::new();
            for saved in &r.saved {
                if let Ok(objects) = git(&path, &["rev-list", "--objects", &saved.commit]) {
                    represented.extend(
                        objects
                            .lines()
                            .filter_map(|line| line.split_whitespace().next().map(str::to_owned)),
                    );
                }
            }
            if let Some(oid) = r
                .unreachable_noncommits
                .iter()
                .find(|oid| !represented.contains(*oid))
            {
                blocker.get_or_insert(format!(
                    "unreachable object not included in pushed recovery commits: {oid}"
                ));
            }
        }
        // Save and push each independently scanned object even if another tip
        // failed its secret scan. `save_object` rescans the exact tip before
        // adding its mapping; therefore an unsafe tip cannot enter this list,
        // while unrelated safe tips can still be recovered. Any failure keeps
        // repository-wide preservation blocked, which prevents cleanup.
        match preserve_saved_lfs(&path, &remote, &r.saved, &r.lfs_files) {
            Ok(objects) => {
                r.lfs_objects = objects;
                r.lfs_preservation = if r.lfs_objects.is_empty() {
                    "not-required".into()
                } else {
                    "push-succeeded".into()
                };
                let mut ref_failures = Vec::new();
                for saved in &mut r.saved {
                    match push_ref(
                        &remote,
                        &path,
                        &saved.commit,
                        &format!("refs/heads/{}", saved.remote_ref),
                    ) {
                        Ok(created) => {
                            saved.created_by_this_run = created;
                            saved.verification = "push-succeeded".into();
                        }
                        Err(error) => {
                            ref_failures.push((
                                format!("push saved {} {}", saved.remote_ref, saved.commit),
                                error,
                            ));
                        }
                    }
                }
                for (context, error) in ref_failures {
                    record_preservation_failure(r, &mut failure, context, error);
                }
            }
            Err(error) => {
                r.lfs_preservation = "blocked".into();
                record_preservation_failure(r, &mut failure, "preserve LFS payloads".into(), error);
            }
        }
        if let Some(error) = failure {
            r.preservation = "blocked".into();
            r.verification_error = Some(error);
        } else if let Some(error) = blocker {
            r.preservation = "blocked".into();
            r.verification_error = Some(error);
        }
        for saved in &mut r.saved {
            if prior_saved.iter().any(|previous| {
                previous.remote_ref == saved.remote_ref
                    && previous.commit == saved.commit
                    && previous.created_by_this_run
            }) {
                saved.created_by_this_run = true;
            }
        }
        for previous in prior_saved {
            if !r.saved.iter().any(|saved| {
                saved.remote_ref == previous.remote_ref && saved.commit == previous.commit
            }) {
                r.saved.push(previous);
            }
        }
    }
    Ok(())
}

fn preserve_branches_only(m: &mut Manifest) -> Result<(), String> {
    let target_remote = m.remote.clone();
    for r in &mut m.repositories {
        if r.deletion == "deleted" && !Path::new(&r.path).exists() {
            continue;
        }
        let prior_saved = r.saved.clone();
        r.existing_refs.clear();
        r.preservation = "blocked".into();
        r.verification_error = None;
        r.preservation_errors.clear();
        if let Some(blocker) = temporary_fixture_blocker(r, &target_remote) {
            r.verification_error = Some(blocker);
            continue;
        }
        if !r.inventory_complete {
            r.verification_error = Some("incomplete Git inventory".into());
            continue;
        }

        let path = PathBuf::from(&r.path);
        match branch_upstreams_target_remote(&path, &m.remote) {
            Ok(true) => {}
            Ok(false) => {
                r.verification_error = Some(
                    "ambiguous matching remote: origin and local branch upstreams do not all map to the requested target".into(),
                );
                continue;
            }
            Err(error) => {
                r.verification_error = Some(format!(
                    "cannot confirm branch push remote mapping: {error}"
                ));
                continue;
            }
        }
        let current_branches = match branch_inventory(&path) {
            Ok(branches) => branches,
            Err(error) => {
                r.verification_error = Some(error);
                continue;
            }
        };
        let mut current_worktrees = match parse_worktrees(&path) {
            Ok(worktrees) => worktrees,
            Err(error) => {
                r.verification_error = Some(error);
                continue;
            }
        };
        let mut worktree_inventory_error = None;
        for worktree in &mut current_worktrees {
            if let Err(error) =
                inventory_worktree(&path, Path::new(&r.common_dir), worktree, &m.remote)
            {
                worktree_inventory_error = Some(error);
                break;
            }
        }
        if let Some(error) = worktree_inventory_error {
            r.verification_error = Some(error);
            continue;
        }
        current_worktrees.sort_by(|left, right| left.path.cmp(&right.path));
        let mut expected_worktrees = r.worktrees.clone();
        expected_worktrees.sort_by(|left, right| left.path.cmp(&right.path));
        if current_branches != r.branches || current_worktrees != expected_worktrees {
            r.verification_error =
                Some("local branches or complete worktree inventory changed since scan".into());
            continue;
        }
        if !r.inventory_errors.is_empty() {
            r.verification_error = Some(format!(
                "incomplete local inventory: {}",
                r.inventory_errors.join("; ")
            ));
            continue;
        }
        if !r.alternates.is_empty() {
            r.verification_error =
                Some("shared Git object storage blocks branch-only cleanup".into());
            continue;
        }
        if !r.lfs_files.is_empty() {
            r.verification_error = Some(
                "Git LFS payloads require full preservation; branch-only cleanup is blocked".into(),
            );
            continue;
        }
        // This mode uploads named local branch tips only. Other local state
        // is explicitly reported as omitted and may be removed by cleanup.
        let branch_oids = r
            .branches
            .iter()
            .map(|branch| branch.commit.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut scan_failures = Vec::new();
        for oid in &branch_oids {
            if let Err(error) = scan_commit(&path, oid, &m.remote) {
                let row = format!("scan branch tip {oid}: {error}");
                scan_failures.push(row.clone());
                r.preservation_errors.push(row);
            }
        }
        if let Some(error) = scan_failures.first() {
            // Secret scanning is an upload gate. Never push a branch after any
            // local tip failed inspection, even if other tips scanned cleanly.
            r.verification_error = Some(error.clone());
            continue;
        }
        r.saved.clear();

        let common = r.common_dir.clone();
        let branches = r.branches.clone();
        let mut failure = None;
        for branch in branches {
            let name = format!("branch:{}", branch.name);
            let reference = recovery_ref(
                &Repository {
                    common_dir: common.clone(),
                    ..Default::default()
                },
                "branch",
                &name,
                &branch.commit,
            );
            match push_ref(
                &m.remote,
                &path,
                &branch.commit,
                &format!("refs/heads/{reference}"),
            ) {
                Ok(created) => {
                    let previously_owned = prior_saved.iter().any(|saved| {
                        saved.source == "branch"
                            && saved.name == name
                            && saved.commit == branch.commit
                            && saved.remote_ref == reference
                            && saved.created_by_this_run
                    });
                    r.saved.push(Saved {
                        source: "branch".into(),
                        name,
                        commit: branch.commit,
                        remote_ref: reference,
                        created_by_this_run: created || previously_owned,
                        retained_ref: None,
                        tree: None,
                        verification: "push-succeeded".into(),
                    });
                }
                Err(error) => {
                    record_preservation_failure(
                        r,
                        &mut failure,
                        format!("push branch {} {}", branch.name, branch.commit),
                        error,
                    );
                }
            }
        }
        for previous in prior_saved {
            if !r.saved.iter().any(|saved| {
                saved.remote_ref == previous.remote_ref && saved.commit == previous.commit
            }) {
                r.saved.push(previous);
            }
        }
        if let Some(error) = failure {
            r.verification_error = Some(error);
        } else {
            r.preservation = "branches-pushed".into();
        }
    }
    Ok(())
}

fn refresh_repository(m: &mut Manifest, requested_path: &Path) -> Result<(), String> {
    let requested_common = common_dir(requested_path)?.to_string_lossy().into_owned();
    let index = m
        .repositories
        .iter()
        .position(|repo| repo.common_dir == requested_common)
        .ok_or_else(|| {
            format!(
                "repository not present in manifest: {}",
                requested_path.display()
            )
        })?;
    let previous = m.repositories[index].clone();
    let owner = if Path::new(&requested_common)
        .file_name()
        .is_some_and(|name| name == ".git")
    {
        Path::new(&requested_common)
            .parent()
            .ok_or("invalid common Git directory")?
            .to_path_buf()
    } else {
        requested_path.to_path_buf()
    };
    let mut refreshed = inventory_one(&owner, previous.matched_paths.clone(), &m.remote);
    if !refreshed.inventory_complete {
        return Err(format!(
            "refreshed inventory incomplete: {}",
            refreshed.inventory_errors.join("; ")
        ));
    }
    refreshed.saved = previous.saved;
    m.repositories[index] = refreshed;
    m.generated_unix = now();
    Ok(())
}

fn save_object(
    remote: &str,
    common: &str,
    r: &mut Repository,
    repo: &Path,
    name: &str,
    oid: &str,
    kind: &str,
) -> Result<(), String> {
    scan_commit(repo, oid, remote)?;
    let rr = recovery_ref(
        &Repository {
            common_dir: common.into(),
            ..Default::default()
        },
        kind,
        name,
        oid,
    );
    r.saved.push(Saved {
        source: kind.into(),
        name: name.into(),
        commit: oid.into(),
        remote_ref: rr,
        created_by_this_run: false,
        retained_ref: None,
        tree: None,
        verification: "pending-push".into(),
    });
    Ok(())
}

fn write_worktree_tree(repo: &Path, head: &str) -> Result<String, String> {
    if git(repo, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
        return if head.is_empty() {
            canonical_empty_tree(repo)
        } else {
            git(repo, &["rev-parse", &format!("{head}^{{tree}}")])
                .map(|tree| tree.trim().to_owned())
        };
    }
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let index = directory.path().join("index");
    let index_path = index.to_str().ok_or("non-UTF8 temporary index")?;
    let envs = [("GIT_INDEX_FILE", index_path)];
    if !head.is_empty() {
        let args = vec![
            "git".into(),
            "-C".into(),
            repo.to_string_lossy().into_owned(),
            "read-tree".into(),
            head.into(),
        ];
        out(&args, None, &envs)?;
    }
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "add".into(),
        "-A".into(),
        "--".into(),
        ".".into(),
    ];
    out(&args, None, &envs)?;
    tree_from_index(repo, Some(&index))
}

/// Return the index tree ID without creating an orphan object for an empty
/// index. Empty trees only need materializing when a saved commit references
/// one; ordinary inventory and comparison must stay read-only.
fn tree_from_index(repo: &Path, index: Option<&Path>) -> Result<String, String> {
    let index_env = index
        .map(|path| {
            path.to_str()
                .map(|path| vec![("GIT_INDEX_FILE", path)])
                .ok_or("non-UTF8 temporary index")
        })
        .transpose()?;
    let envs = index_env.as_deref().unwrap_or(&[]);
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "ls-files".into(),
        "--stage".into(),
    ];
    if out(&args, None, envs)?.trim().is_empty() {
        return canonical_empty_tree(repo);
    }
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "write-tree".into(),
    ];
    Ok(out(&args, None, envs)?.trim().into())
}

fn canonical_empty_tree(repo: &Path) -> Result<String, String> {
    // Compute the tree ID without adding an otherwise unreachable object to
    // the repository. A real empty snapshot still materializes its tree via
    // write-tree and is then preserved through its saved commit.
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let objects = directory.path().join("objects");
    fs::create_dir(&objects).map_err(|error| error.to_string())?;
    let objects = objects
        .to_str()
        .ok_or("non-UTF8 temporary object directory")?;
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "hash-object".into(),
        "-t".into(),
        "tree".into(),
        "--stdin".into(),
    ];
    out(&args, None, &[("GIT_OBJECT_DIRECTORY", objects)]).map(|oid| oid.trim().to_owned())
}

fn commit_snapshot(
    repo: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String, String> {
    let mut args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "-c".into(),
        "user.name=Local Recovery".into(),
        "-c".into(),
        "user.email=recovery@localhost".into(),
        "commit-tree".into(),
        tree.into(),
    ];
    if let Some(parent) = parent {
        args.extend(["-p".into(), parent.into()]);
    }
    args.extend(["-m".into(), message.into()]);
    Ok(out(&args, None, &[])?.trim().into())
}

fn save_worktree(
    remote: &str,
    common: &str,
    repo: &mut Repository,
    wt: &Worktree,
) -> Result<(), String> {
    if wt.bare {
        return Ok(());
    }
    validate_nested_inventory(wt)?;
    let path = Path::new(&wt.path);
    let head = wt.head.as_deref().unwrap_or("");
    let staged = tree_from_index(path, None)?;
    let head_tree = if head.is_empty() {
        canonical_empty_tree(path)?
    } else {
        git(path, &["rev-parse", &format!("{head}^{{tree}}")])?
            .trim()
            .to_owned()
    };
    let full = write_worktree_tree(path, head)?;
    if staged == head_tree && full == head_tree {
        return Ok(());
    }
    let scan = vec![
        "gitleaks".into(),
        "dir".into(),
        "--redact".into(),
        "--no-banner".into(),
        path.to_string_lossy().into_owned(),
    ];
    run_secret_scan(scan, |finding| worktree_finding_bytes(path, finding))?;
    let mut staged_commit = None;
    if staged != head_tree {
        let commit = commit_snapshot(
            path,
            &staged,
            (!head.is_empty()).then_some(head),
            "recovery staged snapshot",
        )?;
        scan_commit_delta(path, &commit, (!head.is_empty()).then_some(head), remote)?;
        let name = format!("staged:{}", wt.path);
        let reference = recovery_ref(
            &Repository {
                common_dir: common.into(),
                ..Default::default()
            },
            "staged",
            &wt.path,
            &commit,
        );
        repo.saved.push(Saved {
            source: "staged-snapshot".into(),
            name,
            commit: commit.clone(),
            remote_ref: reference,
            created_by_this_run: false,
            retained_ref: None,
            tree: Some(staged.clone()),
            verification: "pending-push".into(),
        });
        staged_commit = Some(commit);
    }
    let parent = staged_commit
        .as_deref()
        .or((!head.is_empty()).then_some(head));
    let commit = commit_snapshot(path, &full, parent, "recovery full worktree snapshot")?;
    scan_commit_delta(path, &commit, parent, remote)?;
    let name = format!("worktree:{}", wt.path);
    let reference = recovery_ref(
        &Repository {
            common_dir: common.into(),
            ..Default::default()
        },
        "worktree",
        &wt.path,
        &commit,
    );
    repo.saved.push(Saved {
        source: "worktree-snapshot".into(),
        name,
        commit,
        remote_ref: reference,
        created_by_this_run: false,
        retained_ref: None,
        tree: Some(full),
        verification: "pending-push".into(),
    });
    Ok(())
}

fn same_inventory(expected: &Repository, current: &Repository) -> bool {
    let original_unreachable: BTreeSet<_> = expected.unreachable_commits.iter().collect();
    let current_unreachable: BTreeSet<_> = current.unreachable_commits.iter().collect();
    let saved_commits: BTreeSet<_> = expected.saved.iter().map(|saved| &saved.commit).collect();
    let added_unreachable: BTreeSet<_> = current_unreachable
        .difference(&original_unreachable)
        .copied()
        .collect();
    expected.inventory_complete
        && current.inventory_complete
        && expected.path == current.path
        && expected.common_dir == current.common_dir
        && expected.kind == current.kind
        && expected.branches == current.branches
        && expected.refs == current.refs
        && expected.stashes == current.stashes
        && expected.unreachable_noncommits == current.unreachable_noncommits
        && original_unreachable.is_subset(&current_unreachable)
        && added_unreachable
            .iter()
            .all(|oid| saved_commits.contains(*oid))
        && expected.device == current.device
        && expected.inode == current.inode
        && expected.alternates == current.alternates
        && expected.object_garbage == current.object_garbage
        && expected.worktrees.len() == current.worktrees.len()
        && expected
            .worktrees
            .iter()
            .zip(&current.worktrees)
            .all(|(a, b)| {
                a.path == b.path
                    && a.head == b.head
                    && a.branch == b.branch
                    && a.detached == b.detached
                    && a.bare == b.bare
                    && a.missing == b.missing
                    && a.foreign_registration == b.foreign_registration
                    && a.nested_repositories == b.nested_repositories
                    && a.fingerprint == b.fingerprint
            })
}

fn isolated_verify_saved(remote: &str, saved: &Saved) -> Result<(), String> {
    if !saved.remote_ref.starts_with("recovery/")
        || saved.remote_ref.contains("..")
        || saved.remote_ref.contains(' ')
    {
        return Err(format!("unsafe saved remote ref: {}", saved.remote_ref));
    }
    let remote_ref = format!("refs/heads/{}", saved.remote_ref);
    if remote_oid(remote, &remote_ref)?.as_deref() != Some(saved.commit.as_str()) {
        return Err(format!("remote ref missing or moved: {}", saved.remote_ref));
    }

    // A new bare repository has no access to the source clone's object database.
    let isolated = tempfile::tempdir().map_err(|e| e.to_string())?;
    let repo = isolated.path().join("remote-only.git");
    let repo_s = repo.to_str().ok_or("non-UTF8 temp path")?;
    out(
        &[
            "git".into(),
            "init".into(),
            "--bare".into(),
            "--quiet".into(),
            repo_s.into(),
        ],
        None,
        &[],
    )?;
    out(
        &[
            "git".into(),
            "-C".into(),
            repo_s.into(),
            "remote".into(),
            "add".into(),
            "origin".into(),
            remote.into(),
        ],
        None,
        &[],
    )?;
    let isolated_ref = format!("refs/verify/{}", saved.commit);
    out(
        &[
            "git".into(),
            "-C".into(),
            repo_s.into(),
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "origin".into(),
            format!("{remote_ref}:{isolated_ref}"),
        ],
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )?;
    let fetched = out(
        &[
            "git".into(),
            "--git-dir".into(),
            repo_s.into(),
            "rev-parse".into(),
            "--verify".into(),
            isolated_ref.clone(),
        ],
        None,
        &[],
    )?;
    if fetched.trim() != saved.commit {
        return Err(format!(
            "isolated fetch got unexpected commit for {}",
            saved.remote_ref
        ));
    }
    let alternates = repo.join("objects/info/alternates");
    if alternates.exists() {
        return Err("isolated verification unexpectedly has object alternates".into());
    }
    out(
        &[
            "git".into(),
            "--git-dir".into(),
            repo_s.into(),
            "fsck".into(),
            "--connectivity-only".into(),
            "--no-reflogs".into(),
            saved.commit.clone(),
        ],
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )?;
    let objects = out(
        &[
            "git".into(),
            "--git-dir".into(),
            repo_s.into(),
            "rev-list".into(),
            "--objects".into(),
            "--missing=print".into(),
            saved.commit.clone(),
        ],
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )?;
    if objects.lines().any(|line| line.starts_with('?')) {
        return Err(format!(
            "remote fetch lacks objects for {}",
            saved.remote_ref
        ));
    }
    if let Some(expected_tree) = saved.tree.as_deref() {
        let actual_tree = out(
            &[
                "git".into(),
                "--git-dir".into(),
                repo_s.into(),
                "rev-parse".into(),
                "--verify".into(),
                format!("{}^{{tree}}", saved.commit),
            ],
            None,
            &[],
        )?;
        if actual_tree.trim() != expected_tree {
            return Err(format!(
                "remote snapshot tree mismatch for {}",
                saved.remote_ref
            ));
        }
    }
    if remote_oid(remote, &remote_ref)?.as_deref() != Some(saved.commit.as_str()) {
        return Err(format!(
            "remote ref moved during verification: {}",
            saved.remote_ref
        ));
    }
    Ok(())
}

fn isolated_verify_existing_ref(remote: &str, existing: &ExistingRef) -> Result<(), String> {
    if !existing.reference.starts_with("refs/")
        || existing.reference.contains("..")
        || existing.reference.contains(' ')
        || !matches!(existing.object.len(), 40 | 64)
        || !existing.object.bytes().all(|byte| byte.is_ascii_hexdigit())
        || git(Path::new("."), &["check-ref-format", &existing.reference]).is_err()
    {
        return Err(format!("unsafe existing local ref: {}", existing.reference));
    }
    if remote_oid(remote, &existing.reference)?.as_deref() != Some(existing.object.as_str()) {
        return Err(format!(
            "existing remote ref missing or moved: {}",
            existing.reference
        ));
    }

    // A fresh bare object store fetches the exact ref; no source clone object
    // directory or alternate can satisfy this check.
    let isolated = tempfile::tempdir().map_err(|error| error.to_string())?;
    let repo = isolated.path().join("remote-only.git");
    let repo_s = repo.to_str().ok_or("non-UTF8 temp path")?;
    out(
        &[
            "git".into(),
            "init".into(),
            "--bare".into(),
            "--quiet".into(),
            repo_s.into(),
        ],
        None,
        &[],
    )?;
    out(
        &[
            "git".into(),
            "-C".into(),
            repo_s.into(),
            "remote".into(),
            "add".into(),
            "origin".into(),
            remote.into(),
        ],
        None,
        &[],
    )?;
    let suffix = format!("{:x}", Sha256::digest(existing.reference.as_bytes()));
    let isolated_ref = format!("refs/verify-existing/{suffix}");
    out(
        &[
            "git".into(),
            "-C".into(),
            repo_s.into(),
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "origin".into(),
            format!("{}:{isolated_ref}", existing.reference),
        ],
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )?;
    let fetched = out(
        &[
            "git".into(),
            "--git-dir".into(),
            repo_s.into(),
            "rev-parse".into(),
            "--verify".into(),
            isolated_ref.clone(),
        ],
        None,
        &[],
    )?;
    if fetched.trim() != existing.object {
        return Err(format!(
            "isolated fetch got unexpected object for {}",
            existing.reference
        ));
    }
    if repo.join("objects/info/alternates").exists() {
        return Err("isolated verification unexpectedly has object alternates".into());
    }
    let object_type = out(
        &[
            "git".into(),
            "--git-dir".into(),
            repo_s.into(),
            "cat-file".into(),
            "-t".into(),
            existing.object.clone(),
        ],
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )?;
    if object_type.trim() != existing.object_type {
        return Err(format!(
            "isolated object type differs for {}",
            existing.reference
        ));
    }
    // Full fsck checks object hashes and all objects reachable from the
    // fetched tag/ref, including the commit tree and file blobs.
    out(
        &[
            "git".into(),
            "--git-dir".into(),
            repo_s.into(),
            "fsck".into(),
            "--full".into(),
            "--strict".into(),
            "--no-reflogs".into(),
            existing.object.clone(),
        ],
        None,
        &[("GIT_NO_LAZY_FETCH", "1")],
    )?;
    if remote_oid(remote, &existing.reference)?.as_deref() != Some(existing.object.as_str()) {
        return Err(format!(
            "remote ref moved during verification: {}",
            existing.reference
        ));
    }
    Ok(())
}

fn isolated_verify_saved_lfs(remote: &str, repository: &Repository) -> Result<(), String> {
    let refs = repository
        .saved
        .iter()
        .map(|saved| format!("refs/heads/{}", saved.remote_ref))
        .collect::<Vec<_>>();
    remote_lfs::verify_remote_lfs_refs_match(remote, &refs, &repository.lfs_objects)
}

fn verify_repository(r: &mut Repository, remote: &str) {
    r.verification = "blocked".into();
    r.verification_error = None;
    if let Some(blocker) = temporary_fixture_blocker(r, remote) {
        r.verification_error = Some(blocker);
        return;
    }
    if !r.inventory_complete {
        r.verification_error = Some("incomplete local inventory".into());
        return;
    }
    if r.saved.is_empty() && (!r.branches.is_empty() || !r.stashes.is_empty()) {
        r.verification_error = Some("no recovery refs recorded".into());
        return;
    }
    for saved in &r.saved {
        if let Err(error) = isolated_verify_saved(remote, saved) {
            r.verification_error = Some(error);
            return;
        }
    }
    for existing in &r.existing_refs {
        if let Err(error) = isolated_verify_existing_ref(remote, existing) {
            r.verification_error = Some(error);
            return;
        }
    }
    if let Err(error) = isolated_verify_saved_lfs(remote, r) {
        r.verification_error = Some(error);
        return;
    }
    for saved in &mut r.saved {
        saved.verification = "isolated-verified".into();
    }
    for existing in &mut r.existing_refs {
        existing.verification = "isolated-verified".into();
    }
    r.verification = "isolated-verified".into();
}

fn cleanup_blocker(r: &Repository, _branches_only: bool) -> Option<String> {
    if temporary_recovery_fixture(Path::new(&r.path)) {
        let Some(authorization) = r.temporary_path_authorization.as_ref() else {
            return Some("blocked-temporary-recovery-fixture".into());
        };
        if let Err(error) = temporary_path_authorization(r, &authorization.remote) {
            return Some(format!("blocked-temporary-recovery-fixture:{error}"));
        }
    }
    if !r.inventory_complete {
        return Some("blocked-incomplete-inventory".into());
    }
    if let Some(artifact) = r.object_garbage.first() {
        return Some(format!(
            "blocked-unindexed-object-garbage:{}",
            artifact.path
        ));
    }
    // Branch-only preservation is a push convenience, never deletion proof.
    // Any cleanup mode requires the full inventory and isolated verification.
    if r.preservation != "complete" {
        return Some("blocked-preservation".into());
    }
    if r.saved
        .iter()
        .any(|saved| saved.verification != "isolated-verified")
    {
        return Some("blocked-isolated-verification-incomplete".into());
    }
    if !matches!(
        r.lfs_preservation.as_str(),
        "not-required" | "push-succeeded"
    ) {
        return Some("blocked-LFS-preservation-incomplete".into());
    }
    for worktree in r.worktrees.iter().filter(|worktree| !worktree.bare) {
        let path = Path::new(&worktree.path);
        if !worktree.ignored.is_empty() {
            return Some(format!(
                "blocked-unpreserved-ignored-content:{}",
                worktree.path
            ));
        }
        if worktree.missing || worktree_path_is_absent(path) {
            return Some(format!("blocked-missing-worktree:{}", worktree.path));
        }
        let head_tree = match worktree.head.as_deref() {
            Some(head) => match git(path, &["rev-parse", &format!("{head}^{{tree}}")]) {
                Ok(tree) => tree.trim().to_owned(),
                Err(_) => {
                    return Some(format!(
                        "blocked-worktree-head-tree-unavailable:{}",
                        worktree.path
                    ));
                }
            },
            None => match canonical_empty_tree(path) {
                Ok(tree) => tree,
                Err(_) => {
                    return Some(format!(
                        "blocked-worktree-head-tree-unavailable:{}",
                        worktree.path
                    ));
                }
            },
        };
        let Some(index_tree) = worktree.index_tree.as_deref() else {
            return Some(format!(
                "blocked-worktree-index-tree-unavailable:{}",
                worktree.path
            ));
        };
        let Some(worktree_tree) = worktree.worktree_tree.as_deref() else {
            return Some(format!(
                "blocked-worktree-content-tree-unavailable:{}",
                worktree.path
            ));
        };
        let dirty = worktree.status.iter().any(|line| !line.starts_with('#'))
            || !worktree.untracked.is_empty()
            || !worktree.ignored.is_empty()
            || worktree_tree != head_tree;
        if dirty
            && !r.saved.iter().any(|saved| {
                saved.source == "worktree-snapshot"
                    && saved.name == format!("worktree:{}", worktree.path)
                    && saved.tree.as_deref() == Some(worktree_tree)
                    && saved.verification == "isolated-verified"
            })
        {
            return Some(format!(
                "blocked-unverified-worktree-snapshot:{}",
                worktree.path
            ));
        }
        if index_tree != head_tree
            && !r.saved.iter().any(|saved| {
                saved.source == "staged-snapshot"
                    && saved.name == format!("staged:{}", worktree.path)
                    && saved.tree.as_deref() == Some(index_tree)
                    && saved.verification == "isolated-verified"
            })
        {
            return Some(format!(
                "blocked-unverified-staged-snapshot:{}",
                worktree.path
            ));
        }
    }
    for alternate in &r.alternates {
        let alternate = match Path::new(alternate).canonicalize() {
            Ok(path) if path.is_dir() => path,
            _ => return Some("blocked-shared-storage-unresolved-alternate".into()),
        };
        if alternate.starts_with(Path::new(&r.path)) {
            return Some("blocked-shared-storage-inside-deletion-root".into());
        }
    }
    if r.kind != "clone" && r.kind != "bare" {
        return Some("blocked-repository-kind".into());
    }
    for worktree in &r.worktrees {
        if worktree.missing {
            let path = Path::new(&worktree.path);
            if worktree_path_is_absent(path) {
                return Some(format!("blocked-missing-worktree:{}", worktree.path));
            }
            return Some(format!("blocked-worktree-state-changed:{}", worktree.path));
        }
        if worktree.foreign_registration {
            return Some("blocked-worktree-dependency".into());
        }
        for item in &worktree.nested_repositories {
            if let Some(path) = skipped_tree_path(item) {
                if ensure_no_nested_git(path).is_err() {
                    return Some("blocked-worktree-dependency".into());
                }
            } else if item
                == "<nested-repository scan skipped because ignored content blocks cleanup>"
            {
                if validate_ignored_nested_inventory(worktree).is_err() {
                    return Some("blocked-worktree-dependency".into());
                }
            } else {
                return Some("blocked-worktree-dependency".into());
            }
        }
    }
    for b in &r.branches {
        if !r.saved.iter().any(|s| {
            s.source == "branch"
                && s.name == format!("branch:{}", b.name)
                && s.commit == b.commit
                && s.verification == "isolated-verified"
        }) {
            return Some(format!("blocked-unpushed-branch:{}", b.name));
        }
    }
    for w in &r.worktrees {
        if w.detached
            && !r
                .branches
                .iter()
                .any(|b| Some(&b.commit) == w.head.as_ref())
        {
            let Some(head) = w.head.as_ref() else {
                return Some("blocked-detached-head-missing".into());
            };
            if !r.saved.iter().any(|s| {
                s.source == "detached"
                    && s.name == format!("detached:{}", w.path)
                    && &s.commit == head
                    && s.verification == "isolated-verified"
            }) {
                return Some(format!("blocked-unpushed-detached-head:{}", w.path));
            }
        }
    }
    if !r.stashes.is_empty()
        && r.stashes.iter().any(|stash| {
            let oid = stash.split_whitespace().next().unwrap_or_default();
            !r.saved
                .iter()
                .any(|saved| saved.source == "stash" && saved.commit == oid)
        })
    {
        return Some("blocked-unpreserved-stash".into());
    }
    for worktree in &r.worktrees {
        if worktree.stashes.iter().any(|stash| {
            let oid = stash.split_whitespace().next().unwrap_or_default();
            !r.saved
                .iter()
                .any(|saved| saved.source == "stash" && saved.commit == oid)
        }) {
            return Some(format!(
                "blocked-unpreserved-worktree-stash:{}",
                worktree.path
            ));
        }
    }
    for oid in &r.unreachable_commits {
        if !r.saved.iter().any(|saved| {
            git(
                Path::new(&r.path),
                &["merge-base", "--is-ancestor", oid, &saved.commit],
            )
            .is_ok()
        }) {
            return Some(format!("blocked-unpreserved-unreachable-commit:{oid}"));
        }
    }
    if !r.unreachable_noncommits.is_empty() {
        let represented = r
            .saved
            .iter()
            .flat_map(|saved| {
                git(
                    Path::new(&r.path),
                    &["rev-list", "--objects", &saved.commit],
                )
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
                .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        if let Some(oid) = r
            .unreachable_noncommits
            .iter()
            .find(|oid| !represented.contains(*oid))
        {
            return Some(format!("blocked-unpreserved-unreachable-object:{oid}"));
        }
    }
    if let Some(reference) = unsupported_local_ref(r) {
        return Some(format!("blocked-unpreserved-local-ref:{reference}"));
    }
    let mut refs = BTreeSet::new();
    if r.saved
        .iter()
        .any(|s| !s.remote_ref.starts_with("recovery/") || !refs.insert(&s.remote_ref))
    {
        return Some("blocked-recovery-ref-collision".into());
    }
    None
}

fn unsupported_local_ref(r: &Repository) -> Option<String> {
    r.refs.iter().find_map(|entry| {
        let mut fields = entry.split_whitespace();
        let (Some(reference), Some(oid), Some(kind)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Some(entry.clone());
        };
        if reference.starts_with("refs/heads/") {
            return None;
        }
        if r.existing_refs.iter().any(|existing| {
            existing.reference == reference
                && existing.object == oid
                && existing.object_type == kind
                && existing.verification == "isolated-verified"
        }) {
            return None;
        }
        // Remote-tracking refs are cached views of the target remote. Local-only
        // tracking tips are separately inventoried and saved by preserve().
        if reference.starts_with("refs/remotes/")
            && kind == "commit"
            && r.saved.iter().any(|saved| {
                git(
                    Path::new(&r.path),
                    &["merge-base", "--is-ancestor", oid, &saved.commit],
                )
                .is_ok()
            })
        {
            return None;
        }
        if reference == "refs/stash"
            && kind == "commit"
            && r.saved
                .iter()
                .any(|saved| saved.source == "stash" && saved.commit == oid)
        {
            return None;
        }
        // Tags, notes, replace refs, and unknown namespaces are preserved only
        // when their exact ref/object mapping was fetched and verified remotely.
        Some(reference.to_owned())
    })
}

fn unsupported_local_refs(r: &Repository) -> Vec<(String, String, String)> {
    r.refs
        .iter()
        .filter_map(|entry| {
            let mut fields = entry.split_whitespace();
            let (Some(reference), Some(object), Some(object_type)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return None;
            };
            let supported = reference.starts_with("refs/heads/")
                || reference.starts_with("refs/remotes/")
                || reference == "refs/stash"
                || (reference.starts_with("refs/recovery-local/") && object_type == "commit");
            (!supported).then(|| {
                (
                    reference.to_owned(),
                    object.to_owned(),
                    object_type.to_owned(),
                )
            })
        })
        .collect()
}
fn preview(m: &mut Manifest, branches_only: bool) {
    let deleted = m.deleted.iter().cloned().collect::<BTreeSet<_>>();
    let target_remote = m.remote.clone();
    for r in &mut m.repositories {
        if deleted.contains(&r.path) && !Path::new(&r.path).exists() {
            r.deletion = "deleted".into();
            continue;
        }
        r.deletion = temporary_fixture_blocker(r, &target_remote)
            .or_else(|| cleanup_blocker(r, false))
            .unwrap_or_else(|| "eligible".into());
        if branches_only {
            print_branch_only_omissions(r);
        }
    }
}

fn print_branch_only_omissions(repository: &Repository) {
    for worktree in &repository.worktrees {
        for line in worktree.status.iter().filter(|line| !line.starts_with('#')) {
            println!("omitted-worktree-status\t{}\t{}", worktree.path, line);
        }
        for path in &worktree.untracked {
            println!("omitted-untracked\t{}\t{}", worktree.path, path);
        }
        for path in &worktree.ignored {
            println!("omitted-ignored\t{}\t{}", worktree.path, path);
        }
        for stash in &worktree.stashes {
            println!("omitted-worktree-stash\t{}\t{}", worktree.path, stash);
        }
        if worktree.detached {
            println!(
                "omitted-detached-head\t{}\t{}",
                worktree.path,
                worktree.head.as_deref().unwrap_or("unknown")
            );
        }
    }
    for stash in &repository.stashes {
        println!("omitted-stash\t{}\t{}", repository.path, stash);
    }
    for commit in &repository.unreachable_commits {
        println!(
            "omitted-unreachable-commit\t{}\t{}",
            repository.path, commit
        );
    }
    for object in &repository.unreachable_noncommits {
        println!(
            "omitted-unreachable-object\t{}\t{}",
            repository.path, object
        );
    }
    for reference in &repository.refs {
        if !reference.starts_with("refs/heads/") {
            println!("omitted-local-ref\t{}\t{}", repository.path, reference);
        }
    }
}

fn worktree_path_is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn cleanup_recheck_unreachable(r: &Repository) -> Result<(), String> {
    let owner = Path::new(&r.path);
    let common = Path::new(&r.common_dir);
    let current_garbage = inventory_object_garbage(common)?;
    if current_garbage != r.object_garbage {
        return Err("unindexed Git object-directory garbage changed since inventory".into());
    }
    if let Some(artifact) = current_garbage.first() {
        return Err(format!(
            "unindexed Git object-directory garbage blocks cleanup: {}",
            artifact.path
        ));
    }
    let current_worktrees = parse_worktrees(owner)?;
    let mut current_commits = Vec::new();
    let mut current_noncommits = Vec::new();
    enumerate_unreachable(
        owner,
        &current_worktrees,
        &mut current_commits,
        &mut current_noncommits,
    )?;
    let baseline = r
        .unreachable_commits
        .iter()
        .chain(r.unreachable_noncommits.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut saved_objects = BTreeSet::new();
    for saved in &r.saved {
        saved_objects.insert(saved.commit.clone());
        let objects = git(owner, &["rev-list", "--objects", &saved.commit])?;
        saved_objects.extend(
            objects
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_owned)),
        );
    }
    if let Some(oid) = current_commits
        .iter()
        .chain(current_noncommits.iter())
        .find(|oid| !baseline.contains(*oid) && !saved_objects.contains(*oid))
    {
        return Err(format!(
            "new unpreserved unreachable Git object appeared: {oid}"
        ));
    }
    Ok(())
}

fn cleanup_recheck(r: &Repository, remote: &str) -> Result<(), String> {
    if let Some(blocker) = temporary_fixture_blocker(r, remote) {
        return Err(blocker);
    }
    let owner = Path::new(&r.path);
    if !target_path(owner, remote) {
        return Err("repository remote no longer matches requested target".into());
    }
    let branches = branch_inventory(owner)?;
    if branches != r.branches {
        return Err("local branches changed since inventory".into());
    }
    let refs = git(
        owner,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(objecttype)",
        ],
    )?
    .lines()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if refs != r.refs {
        return Err("local refs changed since inventory".into());
    }
    cleanup_recheck_unreachable(r)?;
    let stashes = if r.kind == "bare" {
        Vec::new()
    } else {
        git(owner, &["stash", "list", "--format=%H %gd %gs"])?
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    if r.kind == "bare" {
        let bare_stashes = git(
            owner,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/stash",
            ],
        )?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        if !stashes.is_empty() || bare_stashes != r.stashes {
            return Err("bare repository stash refs changed since inventory".into());
        }
        let worktrees = parse_worktrees(owner)?;
        if worktrees.len() != r.worktrees.len()
            || worktrees
                .iter()
                .zip(&r.worktrees)
                .any(|(current, expected)| {
                    current.path != expected.path
                        || current.head != expected.head
                        || current.branch != expected.branch
                        || current.bare != expected.bare
                })
        {
            return Err("bare repository worktree registrations changed".into());
        }
        for saved in &r.saved {
            if !saved_commit_is_preserved(remote, saved)? {
                return Err(format!(
                    "pushed ref missing or changed: {}",
                    saved.remote_ref
                ));
            }
        }
        for existing in &r.existing_refs {
            if existing.verification != "isolated-verified"
                || remote_oid(remote, &existing.reference)?.as_deref()
                    != Some(existing.object.as_str())
            {
                return Err(format!(
                    "existing remote ref missing or changed: {}",
                    existing.reference
                ));
            }
        }
        return Ok(());
    }
    if stashes != r.stashes {
        return Err("stash list changed since inventory".into());
    }
    let worktrees = parse_worktrees(owner)?;
    if worktrees.len() != r.worktrees.len() {
        return Err("registered worktrees changed since inventory".into());
    }
    for (current, expected) in worktrees.iter().zip(&r.worktrees) {
        if current.path != expected.path
            || current.head != expected.head
            || current.branch != expected.branch
            || current.detached != expected.detached
            || current.bare != expected.bare
        {
            return Err(format!("worktree registration changed: {}", expected.path));
        }
        validate_nested_inventory(expected)?;
        let path = Path::new(&current.path);
        let status = git(
            path,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        if status != expected.status {
            return Err(format!("worktree status changed: {}", expected.path));
        }
        let untracked = file_list(path, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        let ignored = file_list(
            path,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ],
        )?;
        if untracked != expected.untracked || ignored != expected.ignored {
            return Err(format!(
                "worktree file inventory changed: {}",
                expected.path
            ));
        }
        let mut snapshot = current.clone();
        inventory_worktree(owner, Path::new(&r.common_dir), &mut snapshot, remote)?;
        if snapshot.fingerprint != expected.fingerprint {
            return Err(format!(
                "worktree contents changed since preservation: {}",
                expected.path
            ));
        }
    }
    for saved in &r.saved {
        if !saved_commit_is_preserved(remote, saved)? {
            return Err(format!(
                "pushed ref missing or changed: {}",
                saved.remote_ref
            ));
        }
    }
    for existing in &r.existing_refs {
        if existing.verification != "isolated-verified"
            || remote_oid(remote, &existing.reference)?.as_deref() != Some(existing.object.as_str())
        {
            return Err(format!(
                "existing remote ref missing or changed: {}",
                existing.reference
            ));
        }
    }
    Ok(())
}
fn cleanup_repository(
    r: &Repository,
    remote: &str,
    state: &Path,
    _branches_only: bool,
) -> Result<Vec<String>, String> {
    if let Some(reason) = cleanup_blocker(r, false) {
        return Err(reason);
    }
    for saved in &r.saved {
        isolated_verify_saved(remote, saved)?;
    }
    for existing in &r.existing_refs {
        if existing.verification != "isolated-verified" {
            return Err(format!(
                "existing remote ref lacks isolated verification: {}",
                existing.reference
            ));
        }
        isolated_verify_existing_ref(remote, existing)?;
    }
    let owner = PathBuf::from(&r.path);
    let md = fs::symlink_metadata(&owner).map_err(|e| e.to_string())?;
    if !md.is_dir() || md.file_type().is_symlink() {
        return Err("repository path is not an exact real directory".into());
    }
    cleanup_recheck(r, remote)?;
    let owner_canon = fs::canonicalize(&owner).map_err(|e| e.to_string())?;
    let common_canon = fs::canonicalize(&r.common_dir).map_err(|e| e.to_string())?;
    if !common_canon.starts_with(&owner_canon) {
        return Err("common Git directory is outside owning repository".into());
    }
    verify_filesystem_identity(&owner, r.device.zip(r.inode))?;
    let home = home_directory().ok_or("cannot determine home directory; refusing cleanup")?;
    let home = fs::canonicalize(&home).map_err(|error| {
        format!(
            "cannot canonicalize home directory {}; refusing cleanup: {error}",
            home.display()
        )
    })?;
    if is_filesystem_root(&owner_canon)
        || owner_canon == home
        || owner_canon.starts_with(state)
        || state.starts_with(&owner_canon)
    {
        return Err("unsafe repository/state path relationship".into());
    }
    let mut worktrees: Vec<PathBuf> = r.worktrees.iter().map(|w| PathBuf::from(&w.path)).collect();
    worktrees.sort_by_key(|p| (p == &owner, p.clone()));
    let mut removed = Vec::new();
    // Missing registrations are blocked by cleanup_blocker. Never prune a
    // stale worktree record as a side effect of deleting the owning clone.
    for wt in worktrees {
        if wt == owner {
            continue;
        }
        if !wt.exists() {
            continue;
        }
        let canon = fs::canonicalize(&wt).map_err(|e| e.to_string())?;
        if canon == owner_canon {
            continue;
        }
        if !canon.is_absolute() {
            return Err("unsafe linked worktree path".into());
        }
        let expected = r
            .worktrees
            .iter()
            .find(|record| record.path == wt.to_string_lossy())
            .ok_or("linked worktree absent from inventory")?;
        let live = parse_worktrees(&owner)?;
        if !live.iter().any(|record| {
            record.path == expected.path
                && record.head == expected.head
                && record.branch == expected.branch
                && record.detached == expected.detached
                && !record.missing
        }) {
            return Err(format!(
                "linked worktree registration changed: {}",
                wt.display()
            ));
        }
        let mut current = expected.clone();
        inventory_worktree(&owner, Path::new(&r.common_dir), &mut current, remote)?;
        if current.fingerprint != expected.fingerprint
            || current.status != expected.status
            || current.untracked != expected.untracked
            || current.ignored != expected.ignored
            || current.stashes != expected.stashes
            || current.nested_repositories != expected.nested_repositories
        {
            return Err(format!(
                "linked worktree changed before removal: {}",
                wt.display()
            ));
        }
        validate_nested_inventory(expected)?;
        for saved in &r.saved {
            isolated_verify_saved(remote, saved)?;
        }
        isolated_verify_saved_lfs(remote, r)?;
        cleanup_recheck_unreachable(r)?;
        git(
            &owner,
            &[
                "worktree",
                "remove",
                // This deletes only the already verified local worktree; it does not push.
                // Git requires force here because preserved staged/unstaged files may remain.
                "--force",
                wt.to_str().ok_or("non-UTF8 worktree path")?,
            ],
        )?;
        removed.push(wt.to_string_lossy().into_owned());
    }
    let root_before = r
        .worktrees
        .iter()
        .find(|w| fs::canonicalize(&w.path).ok().as_deref() == Some(owner_canon.as_path()))
        .ok_or("owner root worktree missing from inventory")?;
    let remaining = parse_worktrees(&owner)?;
    if remaining.len() != 1
        || remaining[0].path != root_before.path
        || remaining[0].head != root_before.head
        || remaining[0].branch != root_before.branch
    {
        return Err("repository changed while removing worktrees".into());
    }
    let root = Path::new(&root_before.path);
    if r.kind == "bare" {
        for saved in &r.saved {
            isolated_verify_saved(remote, saved)?;
            if !saved_commit_is_preserved(remote, saved)? {
                return Err(format!(
                    "pushed ref changed immediately before deletion: {}",
                    saved.remote_ref
                ));
            }
        }
        isolated_verify_saved_lfs(remote, r)?;
        cleanup_recheck_unreachable(r)?;
        verify_filesystem_identity(&owner, r.device.zip(r.inode))?;
        fs::remove_dir_all(&owner)
            .map_err(|e| format!("remove exact bare repository {}: {e}", owner.display()))?;
        removed.push(owner.to_string_lossy().into_owned());
        return Ok(removed);
    }
    let status = git(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?
    .lines()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let untracked = file_list(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let ignored = file_list(
        root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    if status != root_before.status
        || untracked != root_before.untracked
        || ignored != root_before.ignored
    {
        return Err("owner worktree changed while removing linked worktrees".into());
    }
    let mut final_snapshot = root_before.clone();
    inventory_worktree(
        &owner,
        Path::new(&r.common_dir),
        &mut final_snapshot,
        remote,
    )?;
    if final_snapshot.fingerprint != root_before.fingerprint {
        return Err("owner worktree contents changed immediately before deletion".into());
    }
    for saved in &r.saved {
        if !saved_commit_is_preserved(remote, saved)? {
            return Err(format!(
                "pushed ref changed immediately before deletion: {}",
                saved.remote_ref
            ));
        }
    }
    isolated_verify_saved_lfs(remote, r)?;
    cleanup_recheck_unreachable(r)?;
    verify_filesystem_identity(&owner, r.device.zip(r.inode))?;
    fs::remove_dir_all(&owner)
        .map_err(|e| format!("remove exact clone {}: {e}", owner.display()))?;
    removed.push(owner.to_string_lossy().into_owned());
    Ok(removed)
}
fn dedupe(m: &mut Manifest, state: &Path, execute: bool) -> Result<(), String> {
    dedupe_with_options(m, state, execute, false, false)
}

fn dedupe_with_options(
    m: &mut Manifest,
    _state: &Path,
    execute: bool,
    all_unprotected: bool,
    json: bool,
) -> Result<(), String> {
    if execute {
        return Err("dedupe --execute disabled: remote branch deletion is forbidden".into());
    }
    let remote = m.remote.clone();
    reject_remote_url_rewrite(&remote, None)?;
    let snapshot = remote_snapshot::fetch_remote_history(&remote)?;
    let branch_facts = snapshot
        .branches
        .iter()
        .map(|branch| (branch.name.clone(), branch.clone()))
        .collect::<BTreeMap<_, _>>();
    let default_branch = remote_snapshot::default_branch(&remote)?
        .ok_or_else(|| "remote default branch is unknown; refusing dedupe".to_owned())?;
    let owned = m
        .repositories
        .iter()
        .flat_map(|repository| repository.saved.iter())
        .chain(m.recovery_ownership.iter())
        .filter(|saved| {
            saved.source == "branch"
                && saved.remote_ref.starts_with("recovery/find-and-recovery/")
                && branch_facts
                    .get(&saved.remote_ref)
                    .is_some_and(|branch| branch.oid == saved.commit)
        })
        .map(|saved| saved.remote_ref.clone())
        .collect::<BTreeSet<_>>();
    let eligible = if all_unprotected {
        branch_facts.keys().cloned().collect()
    } else {
        owned
    };
    // Protection and active PR facts require authenticated GitHub metadata.
    // Until that identity-bound API inventory is available, this plan is
    // informational and can never authorize a deletion.
    let protected = BTreeSet::new();
    let plan = dedupe::plan_dedupe(
        snapshot.branches,
        &snapshot.commit_parents,
        &eligible,
        &protected,
        &default_branch,
    )?;
    if json {
        let report = serde_json::json!({
            "schema_version": 1,
            "default_branch": default_branch,
            "scope": if all_unprotected { "all_unprotected" } else { "tool_owned_recovery_branches" },
            "execution_authorized": false,
            "execution_blocker": "server protection and active pull request facts are not verified; remote deletion is disabled",
            "plan": plan,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        println!("default-branch\t{default_branch}");
        println!(
            "scope\t{}",
            if all_unprotected {
                "all-unprotected"
            } else {
                "tool-owned-recovery-branches"
            }
        );
        println!(
            "execution-blocked\tserver protection and active pull request facts are not verified; remote deletion is disabled"
        );
        for name in &plan.retained_branches {
            if let Some(branch) = branch_facts.get(name) {
                println!("retain\t{name}\t{}", branch.oid);
            }
        }
        for candidate in &plan.deletions {
            println!(
                "candidate\t{}\t{}\t{}\t{}\t{}\t{}",
                candidate.branch,
                candidate.expected_oid,
                candidate.relation.as_str(),
                candidate.survivor_branch,
                candidate.survivor_oid,
                candidate.reason,
            );
        }
    }
    Ok(())
}
fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let state = cli
        .state
        .clone()
        .map(Ok)
        .unwrap_or_else(default_state_dir)?;
    let state = state.canonicalize().unwrap_or(state);
    let previous_scan_manifest =
        if matches!(&cli.command, Phase::Scan { .. }) && state.join("manifest.json").is_file() {
            let previous = load(&state)?;
            if canon_url(&cli.remote) != canon_url(&previous.remote) {
                return Err("manifest remote differs; use separate state".into());
            }
            Some(previous)
        } else {
            None
        };
    let mut m = match &cli.command {
        Phase::Scan { .. } => Manifest {
            schema_version: 1,
            remote: cli.remote.clone(),
            generated_unix: now(),
            ..Default::default()
        },
        _ => load(&state)?,
    };
    if let Some(previous) = &previous_scan_manifest {
        merge_recovery_ownership(&mut m, previous);
        merge_deletion_history(&mut m, previous);
    }
    if canon_url(&cli.remote) != canon_url(&m.remote) {
        return Err("manifest remote differs; use separate state".into());
    }
    match cli.command {
        Phase::Scan {
            roots,
            root_list,
            exhaustive,
            allow_temp_repositories,
        } => {
            let mut roots = roots;
            if let Some(list) = root_list {
                roots.extend(read_root_list(&list)?);
            }
            let roots = if roots.is_empty() {
                default_scan_roots(exhaustive)
            } else {
                roots
            };
            m.roots = roots
                .iter()
                .map(|x| x.to_string_lossy().into_owned())
                .collect();
            m.repositories.clear();
            m.coverage_gaps.clear();
            m.generated_unix = now();
            save(&m, &state)?;
            let remote = m.remote.clone();
            let (mut r, g) = discover(&roots, &remote, |partial, gaps| {
                m.repositories = partial.to_vec();
                m.coverage_gaps = gaps.to_vec();
                m.generated_unix = now();
                save(&m, &state)
            })?;
            authorize_temporary_repositories(&mut r, &allow_temp_repositories, &remote)?;
            m.repositories = r;
            m.coverage_gaps = g;
            for skipped in [
                "/Applications",
                "/Library",
                "/System",
                "/usr",
                "/bin",
                "/sbin",
                "/pkg",
                "/cores",
                "/private/etc",
                "/private/var",
                "/private/tftpboot",
            ] {
                m.coverage_gaps.push(format!(
                    "scan root omitted to avoid traversing system data: {skipped}"
                ));
            }
            m.generated_unix = now();
            save(&m, &state)?;
            println!("{}", serde_json::to_string_pretty(&m).unwrap());
        }
        Phase::Preserve { branches_only } => {
            if branches_only {
                preserve_branches_only(&mut m)?;
            } else {
                preserve(&mut m)?;
            }
            save(&m, &state)?;
            let blocked = m
                .repositories
                .iter()
                .filter(|repository| repository.preservation != "complete")
                .count();
            println!("preservation recorded; inspect manifest");
            if blocked > 0 {
                return Err(format!(
                    "preservation incomplete: {blocked} repository copies blocked"
                ));
            }
        }
        Phase::Verify => {
            for repository in &mut m.repositories {
                verify_repository(repository, &m.remote);
                println!("{}\t{}", repository.verification, repository.path);
                if let Some(error) = &repository.verification_error {
                    println!("  {error}");
                }
            }
            m.generated_unix = now();
            save(&m, &state)?;
            let blocked = m
                .repositories
                .iter()
                .filter(|repository| repository.verification != "isolated-verified")
                .count();
            if blocked > 0 {
                return Err(format!(
                    "verification incomplete: {blocked} repository copies blocked"
                ));
            }
        }
        Phase::Refresh { path } => {
            refresh_repository(&mut m, &path)?;
            save(&m, &state)?;
            println!("refreshed\t{}", path.display());
        }
        Phase::Preview { branches_only } => {
            preview(&mut m, branches_only);
            save(&m, &state)?;
            for r in &m.repositories {
                println!("{}\t{}", r.deletion, r.path)
            }
        }
        Phase::Cleanup {
            execute,
            branches_only,
        }
        | Phase::Resume {
            execute,
            branches_only,
        } => {
            preview(&mut m, branches_only);
            if !execute {
                for r in &m.repositories {
                    if r.deletion == "eligible" {
                        println!("{}", r.path)
                    }
                }
            } else {
                // Process serially. Each exact candidate is re-inventoried immediately
                // before removing linked worktrees, then its owning clone.
                let mut blocked = 0usize;
                for i in 0..m.repositories.len() {
                    if m.repositories[i].deletion != "eligible" {
                        println!("{}\t{}", m.repositories[i].deletion, m.repositories[i].path);
                        if m.repositories[i].deletion != "deleted" {
                            blocked += 1;
                        }
                        continue;
                    }
                    // Cleanup always uses strict full-preservation behavior.
                    // `--branches-only` affects preserve/preview output only.
                    match cleanup_repository(&m.repositories[i], &m.remote, &state, false) {
                        Ok(paths) => {
                            m.repositories[i].deletion = "deleted".into();
                            let repository = m.repositories[i].clone();
                            record_deleted_copy(&mut m, &repository, &paths);
                        }
                        Err(e) => {
                            m.repositories[i].deletion = format!("blocked:{e}");
                            println!("{}\t{}", m.repositories[i].deletion, m.repositories[i].path);
                            blocked += 1;
                        }
                    }
                    save(&m, &state)?;
                }
                if blocked > 0 {
                    return Err(format!(
                        "cleanup incomplete: {blocked} repository copies blocked"
                    ));
                }
                if !m.coverage_gaps.is_empty() {
                    return Err(format!(
                        "cleanup completed for known copies, but discovery has {} coverage gaps",
                        m.coverage_gaps.len()
                    ));
                }
                println!("cleanup recorded; inspect manifest");
            }
        }
        Phase::Dedupe {
            execute,
            all_unprotected,
            json,
        } => dedupe_with_options(&mut m, &state, execute, all_unprotected, json)?,
        Phase::ResumePartial { execute } => {
            require_partial_resume_preview_only(execute)?;
            preview(&mut m, true);
            for repository in &m.repositories {
                println!("{}\t{}", repository.deletion, repository.path);
            }
        }
        Phase::RecordRecoveredDeletion { path, ref_prefix } => {
            record_recovered_deletion(&mut m, &path, &ref_prefix)?;
            save(&m, &state)?;
            println!(
                "reconstructed deletion mapping recorded for {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod preservation_tests {
    use super::*;

    #[test]
    fn partial_resume_execution_is_rejected() {
        assert!(require_partial_resume_preview_only(false).is_ok());
        assert!(
            require_partial_resume_preview_only(true)
                .unwrap_err()
                .contains("resume-partial --execute is disabled")
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_follows_explicit_symlink_root_and_finds_repo_under_generated_names() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let actual_root = temp.path().join("actual");
        let scan_alias = temp.path().join("tmp-alias");
        let repo = actual_root.join("target").join("build").join("clone");
        fs::create_dir_all(repo.parent().unwrap()).unwrap();
        symlink(&actual_root, &scan_alias).unwrap();
        cmd(&["git", "init", repo.to_str().unwrap()]);
        cmd(&[
            "git",
            "-C",
            repo.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "https://github.com/donbeave/terminal-components-claude",
        ]);

        let roots = [scan_alias, actual_root];
        let (repositories, gaps) = discover(
            &roots,
            "https://github.com/donbeave/terminal-components-claude",
            |_, _| Ok(()),
        )
        .unwrap();

        assert!(gaps.is_empty(), "unexpected scan gaps: {gaps:?}");
        assert_eq!(repositories.len(), 1);
        assert_eq!(
            Path::new(&repositories[0].path),
            repo.canonicalize().unwrap()
        );
    }

    #[test]
    fn clean_overlapping_tips_use_one_batched_scan() {
        let oids = vec!["a".into(), "b".into(), "a".into()];
        let mut calls = Vec::new();
        let results = scan_tip_set(&oids, |tips| {
            calls.push(tips.to_vec());
            Ok(())
        });

        assert_eq!(calls, vec![vec!["a".to_string(), "b".to_string()]]);
        assert!(results.values().all(Result::is_ok));
    }

    #[test]
    fn unsupported_remote_refs_are_queried_in_bounded_batches() {
        let references = (0..(REMOTE_REF_QUERY_BATCH_SIZE * 2 + 1))
            .map(|index| format!("refs/tags/local-{index:04}"))
            .collect::<Vec<_>>();
        let mut batch_sizes = Vec::new();
        let remote = remote_oids_for_refs_with(&references, |batch| {
            batch_sizes.push(batch.len());
            Ok(batch
                .iter()
                .map(|reference| format!("{}\t{reference}", "a".repeat(40)))
                .collect::<Vec<_>>()
                .join("\n"))
        })
        .unwrap();

        assert_eq!(
            batch_sizes,
            [REMOTE_REF_QUERY_BATCH_SIZE, REMOTE_REF_QUERY_BATCH_SIZE, 1]
        );
        assert_eq!(remote.len(), references.len());
        assert!(references.iter().all(|reference| {
            remote
                .get(reference)
                .is_some_and(|oid| oid == &"a".repeat(40))
        }));
    }

    #[test]
    fn failed_batch_falls_back_and_keeps_secret_tip_blocked_only() {
        let oids = vec!["clean".into(), "secret".into(), "clean".into()];
        let mut calls = Vec::new();
        let results = scan_tip_set(&oids, |tips| {
            calls.push(tips.to_vec());
            match tips {
                [oid] if oid == "secret" => Err("finding detected".into()),
                [_] => Ok(()),
                _ => Err("batch contains finding".into()),
            }
        });

        assert_eq!(
            calls,
            vec![
                vec!["clean".to_string(), "secret".to_string()],
                vec!["clean".to_string()],
                vec!["secret".to_string()],
            ]
        );
        assert!(results["clean"].is_ok());
        assert!(results["secret"].is_err());
    }

    #[test]
    fn full_preserve_guard_rejects_target_origin_with_foreign_branch_upstream() {
        let (_temp, _remote, local, remote_s) = fixture();
        let foreign = local.parent().unwrap().join("foreign.git");
        cmd(&["git", "init", "--bare", foreign.to_str().unwrap()]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "remote",
            "add",
            "jackin",
            foreign.to_str().unwrap(),
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "config",
            "branch.main.remote",
            "jackin",
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "config",
            "branch.main.merge",
            "refs/heads/main",
        ]);
        assert_eq!(
            branch_upstreams_target_remote(&local, &remote_s).unwrap(),
            false
        );
    }

    fn cmd(args: &[&str]) -> String {
        let o = Command::new(args[0])
            .args(&args[1..])
            .output()
            .expect("test command");
        assert!(
            o.status.success(),
            "{}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let t = tempfile::tempdir().unwrap();
        let remote = t.path().join("remote.git");
        let local = t.path().join("local");
        cmd(&["git", "init", "--bare", remote.to_str().unwrap()]);
        cmd(&["git", "init", "-b", "main", local.to_str().unwrap()]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "config",
            "user.name",
            "Test",
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.invalid",
        ]);
        fs::write(local.join("tracked.txt"), "base\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "tracked.txt"]);
        cmd(&["git", "-C", local.to_str().unwrap(), "commit", "-m", "base"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            remote.to_str().unwrap(),
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "push",
            "origin",
            "main",
        ]);
        let remote_s = remote.to_string_lossy().to_string();
        (t, local, remote, remote_s)
    }
    fn unborn_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let t = tempfile::tempdir().unwrap();
        let remote = t.path().join("remote.git");
        let local = t.path().join("local");
        cmd(&["git", "init", "--bare", remote.to_str().unwrap()]);
        cmd(&["git", "init", "-b", "main", local.to_str().unwrap()]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            remote.to_str().unwrap(),
        ]);
        let remote_s = remote.to_string_lossy().to_string();
        (t, local, remote, remote_s)
    }

    #[test]
    fn unborn_inventory_computes_empty_tree_without_adding_an_object() {
        let (_temp, local, _remote, remote_s) = unborn_fixture();
        let empty_tree = canonical_empty_tree(&local).unwrap();
        let mut manifest = manifest(&local, &remote_s);
        // Git treats the canonical empty-tree OID as a built-in object, so
        // `cat-file -e` succeeds without a loose object being present.
        let objects = git(
            &local,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        )
        .unwrap();
        assert!(!objects.lines().any(|oid| oid == empty_tree));
        assert!(
            manifest.repositories[0]
                .unreachable_noncommits
                .iter()
                .all(|oid| oid != &empty_tree)
        );
    }

    #[test]
    fn empty_index_tree_comparison_does_not_add_an_unreachable_tree() {
        let (_temp, local, _remote, _remote_s) = unborn_fixture();
        let empty_tree = canonical_empty_tree(&local).unwrap();

        assert_eq!(tree_from_index(&local, None).unwrap(), empty_tree);
        assert_eq!(write_worktree_tree(&local, "").unwrap(), empty_tree);
        // `cat-file -e` recognizes the canonical empty-tree OID even when no
        // object was written. Check the actual object inventory instead.
        let objects = git(
            &local,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        )
        .unwrap();
        assert!(!objects.lines().any(|oid| oid == empty_tree));
    }

    #[test]
    fn saved_empty_snapshot_references_the_canonical_empty_tree() {
        let (_temp, local, _remote, remote_s) = unborn_fixture();
        let empty_tree = canonical_empty_tree(&local).unwrap();
        let snapshot_tree = write_worktree_tree(&local, "").unwrap();
        assert_eq!(snapshot_tree, empty_tree);

        let snapshot =
            commit_snapshot(&local, &snapshot_tree, None, "empty recovery snapshot").unwrap();
        push_ref(
            &remote_s,
            &local,
            &snapshot,
            "refs/heads/recovery/empty-snapshot",
        )
        .unwrap();
        let closure = git(&local, &["rev-list", "--objects", &snapshot]).unwrap();

        assert!(
            closure
                .lines()
                .any(|line| line.split_whitespace().next() == Some(empty_tree.as_str()))
        );
    }

    #[test]
    fn preexisting_orphan_empty_tree_blocks_until_saved_commit_references_it() {
        let (_temp, local, _remote, remote_s) = unborn_fixture();
        let empty_tree = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "hash-object",
            "-w",
            "-t",
            "tree",
            "--stdin",
        ]);
        let mut manifest = manifest(&local, &remote_s);
        assert!(
            manifest.repositories[0]
                .unreachable_noncommits
                .contains(&empty_tree)
        );

        preserve(&mut manifest).unwrap();
        let repository = &mut manifest.repositories[0];
        assert_eq!(repository.preservation, "blocked");
        assert!(
            repository
                .verification_error
                .as_deref()
                .is_some_and(|error| error.contains(&empty_tree))
        );

        let snapshot =
            commit_snapshot(&local, &empty_tree, None, "empty recovery snapshot").unwrap();
        let remote_ref = "recovery/empty-snapshot";
        push_ref(
            &remote_s,
            &local,
            &snapshot,
            &format!("refs/heads/{remote_ref}"),
        )
        .unwrap();
        repository.saved.push(Saved {
            source: "worktree-snapshot".into(),
            name: format!("worktree:{}", local.display()),
            commit: snapshot,
            remote_ref: remote_ref.into(),
            created_by_this_run: true,
            retained_ref: None,
            tree: Some(empty_tree.clone()),
            verification: "isolated-verified".into(),
        });
        repository.preservation = "complete".into();
        repository.lfs_preservation = "not-required".into();

        let empty_tree_blocker = format!("blocked-unpreserved-unreachable-object:{empty_tree}");
        assert_ne!(
            cleanup_blocker(repository, false).as_deref(),
            Some(empty_tree_blocker.as_str())
        );
    }

    fn unborn_root_commit(local: &Path, filename: &str, content: &str) -> String {
        fs::write(local.join(filename), content).unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", filename]);
        let tree = cmd(&["git", "-C", local.to_str().unwrap(), "write-tree"]);
        commit_snapshot(local, &tree, None, "unborn recovery root").unwrap()
    }

    #[test]
    fn parentless_unborn_commit_pushes_to_an_empty_remote_ref() {
        let (_temp, local, remote, remote_s) = unborn_fixture();
        let commit = unborn_root_commit(&local, "notes.txt", "unborn worktree data\n");
        assert!(commit_has_no_parent(&local, &commit).unwrap());

        assert!(
            push_ref(
                &remote_s,
                &local,
                &commit,
                "refs/heads/recovery/unborn-root"
            )
            .unwrap()
        );
        assert_eq!(
            remote_oid(&remote_s, "refs/heads/recovery/unborn-root")
                .unwrap()
                .as_deref(),
            Some(commit.as_str())
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "show",
                &format!("{commit}:notes.txt"),
            ]),
            "unborn worktree data"
        );
    }

    #[test]
    fn parentless_unborn_push_rejects_competing_unrelated_ref() {
        let (_temp, local, remote, remote_s) = unborn_fixture();
        let commit = unborn_root_commit(&local, "notes.txt", "unborn worktree data\n");
        let unrelated_tree = canonical_empty_tree(&local).unwrap();
        let competing = commit_snapshot(&local, &unrelated_tree, None, "unrelated root").unwrap();
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "push",
            &remote_s,
            &format!("{competing}:refs/heads/competitor"),
        ]);
        let target = "refs/heads/recovery/unborn-root";
        let zeros = "0".repeat(competing.len());
        let update = [
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "update-ref",
            target,
            &competing,
            &zeros,
        ];
        cmd(&update);

        let error = push_ref(&remote_s, &local, &commit, target).unwrap_err();
        assert!(error.contains("remote ref collision"), "{error}");
        assert_eq!(
            remote_oid(&remote_s, target).unwrap().as_deref(),
            Some(competing.as_str())
        );
    }

    #[test]
    fn parented_commit_without_remote_ancestor_does_not_use_create_only_fallback() {
        let (_temp, local, remote, remote_s) = unborn_fixture();
        let root = unborn_root_commit(&local, "first.txt", "first\n");
        let tree = canonical_empty_tree(&local).unwrap();
        let child = commit_snapshot(&local, &tree, Some(&root), "parented recovery").unwrap();
        assert!(!commit_has_no_parent(&local, &child).unwrap());

        let error = push_ref(
            &remote_s,
            &local,
            &child,
            "refs/heads/recovery/parented-without-ancestor",
        )
        .unwrap_err();
        assert!(
            error.contains("no fetched remote commit is a proven ancestor"),
            "{error}"
        );
        assert_eq!(
            remote_oid(&remote_s, "refs/heads/recovery/parented-without-ancestor").unwrap(),
            None
        );
    }

    #[test]
    fn unborn_untracked_content_is_inventoried_and_preserved_without_parent() {
        let (_temp, local, remote, remote_s) = unborn_fixture();
        fs::write(local.join("notes.txt"), "unborn worktree data\n").unwrap();

        let mut manifest = manifest(&local, &remote_s);
        let worktree = &manifest.repositories[0].worktrees[0];
        assert_eq!(worktree.head, None);
        assert_eq!(worktree.branch.as_deref(), Some("main"));
        assert_eq!(worktree.untracked, vec!["notes.txt"]);
        assert!(worktree.worktree_tree.is_some());

        preserve(&mut manifest).unwrap();
        let repository = &manifest.repositories[0];
        assert_eq!(
            repository.preservation, "complete",
            "{:?}; {:?}",
            repository.preservation_errors, repository.verification_error
        );
        let snapshot = repository
            .saved
            .iter()
            .find(|saved| saved.source == "worktree-snapshot")
            .expect("unborn untracked content must have a recovery snapshot");
        assert_eq!(
            snapshot.tree.as_deref(),
            repository.worktrees[0].worktree_tree.as_deref()
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "cat-file",
                "-p",
                &snapshot.commit,
            ])
            .lines()
            .filter(|line| line.starts_with("parent "))
            .count(),
            0,
            "unborn snapshot must remain parentless"
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "show",
                &format!("{}:notes.txt", snapshot.commit),
            ]),
            "unborn worktree data"
        );
    }

    #[test]
    fn unborn_worktree_recheck_detects_same_status_with_changed_content() {
        let (_temp, local, _remote, remote_s) = unborn_fixture();
        fs::write(local.join("notes.txt"), "before\n").unwrap();
        let mut manifest = manifest(&local, &remote_s);
        preserve(&mut manifest).unwrap();
        assert_eq!(
            manifest.repositories[0].preservation, "complete",
            "{:?}",
            manifest.repositories[0].verification_error
        );

        let status = manifest.repositories[0].worktrees[0].status.clone();
        fs::write(local.join("notes.txt"), "after\n").unwrap();
        let current = inventory_one(
            &local,
            vec![local.to_string_lossy().into_owned()],
            &remote_s,
        );
        assert_eq!(
            current.worktrees[0].status, status,
            "status must be unchanged"
        );
        assert_ne!(
            current.worktrees[0].fingerprint, manifest.repositories[0].worktrees[0].fingerprint,
            "content change must alter the cleanup fingerprint"
        );
        let error = cleanup_recheck(&manifest.repositories[0], &remote_s).unwrap_err();
        assert!(
            error.contains("worktree contents changed")
                || error.contains("new unpreserved unreachable Git object appeared"),
            "changed bytes must fail cleanup recheck: {error}"
        );
    }

    #[test]
    fn unborn_staged_and_worktree_snapshots_remain_distinct() {
        let (_temp, local, remote, remote_s) = unborn_fixture();
        fs::write(local.join("staged.txt"), "staged version\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "staged.txt"]);
        fs::write(local.join("staged.txt"), "working version\n").unwrap();

        let mut manifest = manifest(&local, &remote_s);
        preserve(&mut manifest).unwrap();
        let repository = &manifest.repositories[0];
        assert_eq!(
            repository.preservation, "complete",
            "{:?}; {:?}",
            repository.preservation_errors, repository.verification_error
        );
        let staged = repository
            .saved
            .iter()
            .find(|saved| saved.source == "staged-snapshot")
            .expect("distinct staged content must be preserved");
        let full = repository
            .saved
            .iter()
            .find(|saved| saved.source == "worktree-snapshot")
            .expect("working content must be preserved separately");
        assert_ne!(staged.tree, full.tree);
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "show",
                &format!("{}:staged.txt", staged.commit),
            ]),
            "staged version"
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "show",
                &format!("{}:staged.txt", full.commit),
            ]),
            "working version"
        );
    }

    #[test]
    fn detached_zero_head_is_rejected_as_corrupt_metadata() {
        let (_temp, local, _remote, remote_s) = unborn_fixture();
        let zero_head = format!("{}\n", "0".repeat(object_id_length(&local).unwrap()));
        fs::write(local.join(".git/HEAD"), zero_head).unwrap();

        let inventory = inventory_one(
            &local,
            vec![local.to_string_lossy().into_owned()],
            &remote_s,
        );
        assert!(!inventory.inventory_complete);
        assert!(inventory.inventory_errors.iter().any(|error| {
            error.contains("zero worktree HEAD is not attached to a symbolic branch")
        }));
        assert!(inventory.worktrees.is_empty());
    }

    fn manifest(local: &Path, remote: &str) -> Manifest {
        let r = inventory_one(local, vec![local.to_string_lossy().into_owned()], remote);
        assert!(r.inventory_complete, "{:?}", r.inventory_errors);
        Manifest {
            schema_version: 1,
            remote: remote.into(),
            repositories: vec![r],
            ..Default::default()
        }
    }

    #[test]
    fn preserve_blocks_disappeared_candidate_and_continues_other_repositories() {
        let (_temp, stale, remote, remote_s) = fixture();
        let second = stale.parent().unwrap().join("second-clone");
        cmd(&[
            "git",
            "clone",
            remote.to_str().unwrap(),
            second.to_str().unwrap(),
        ]);
        let stale_inventory = inventory_one(
            &stale,
            vec![stale.to_string_lossy().into_owned()],
            &remote_s,
        );
        let second_inventory = inventory_one(
            &second,
            vec![second.to_string_lossy().into_owned()],
            &remote_s,
        );
        assert!(stale_inventory.inventory_complete);
        assert!(second_inventory.inventory_complete);
        fs::remove_dir_all(&stale).unwrap();

        let mut manifest = Manifest {
            schema_version: 1,
            remote: remote_s.clone(),
            repositories: vec![stale_inventory, second_inventory],
            ..Default::default()
        };
        preserve(&mut manifest).unwrap();

        assert_eq!(manifest.repositories[0].preservation, "blocked");
        assert!(
            manifest.repositories[0]
                .verification_error
                .as_deref()
                .is_some_and(|error| error.contains("candidate path unavailable"))
        );
        assert_eq!(manifest.repositories[1].preservation, "complete");
        assert!(!manifest.repositories[1].saved.is_empty());
    }

    fn fetched_tree(remote: &Path, oid: &str, path: &str) -> String {
        cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "show",
            &format!("{oid}:{path}"),
        ])
    }

    #[test]
    fn recovery_push_rejects_existing_ref_without_advancing_it() {
        let (_temp, local, remote, remote_s) = fixture();
        let before = cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ]);
        fs::write(local.join("second.txt"), "second\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "second.txt"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit",
            "-m",
            "second",
        ]);
        let new_oid = cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", "HEAD"]);
        let error = push_ref(&remote_s, &local, &new_oid, "refs/heads/main").unwrap_err();
        assert!(error.contains("remote ref collision"));
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ]),
            before
        );
    }

    #[test]
    fn recovery_push_rejects_ref_created_after_absence_preflight() {
        let (_temp, local, remote, remote_s) = fixture();
        let reference = "refs/heads/recovery/race";
        assert_eq!(remote_oid(&remote_s, reference).unwrap(), None);

        // Model another writer winning after the caller's absence preflight,
        // inside the create callback where the real API call races. We must
        // not fall back to a Git push or advance the competing ref.
        let ancestor = cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ]);
        fs::write(local.join("second.txt"), "second\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "second.txt"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit",
            "-m",
            "second",
        ]);
        let descendant = cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", "HEAD"]);

        let calls = std::cell::Cell::new(0);
        let result = push_ref_with_create(&remote_s, &local, &descendant, reference, |_, _, _| {
            calls.set(calls.get() + 1);
            let zeros = "0".repeat(ancestor.len());
            let args = [
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "update-ref",
                reference,
                &ancestor,
                &zeros,
            ];
            let output = Command::new(args[0]).args(&args[1..]).output().unwrap();
            assert!(output.status.success(), "competing create must win");
            Err("create-ref rejected: reference already exists".into())
        });
        assert!(
            result.is_err(),
            "create collision must block the save: {result:?}"
        );
        assert_eq!(calls.get(), 1, "result before create callback: {result:?}");
        assert_eq!(
            remote_oid(&remote_s, reference).unwrap().as_deref(),
            Some(ancestor.as_str())
        );
    }

    #[test]
    fn pushes_branch_and_dirty_worktree_snapshot_before_cleanup() {
        let (_t, local, remote, remote_s) = fixture();
        fs::write(
            local.join("tracked.txt"),
            "local uncommitted content
",
        )
        .unwrap();
        fs::write(
            local.join("untracked.txt"),
            "untracked payload
",
        )
        .unwrap();
        let original_main = cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(
            m.repositories[0].preservation, "complete",
            "{:?}",
            m.repositories[0].verification_error
        );
        assert!(
            m.repositories[0]
                .saved
                .iter()
                .any(|saved| saved.source == "branch")
        );
        let worktree = m.repositories[0]
            .saved
            .iter()
            .find(|saved| saved.source == "worktree-snapshot")
            .unwrap_or_else(|| panic!("missing worktree snapshot: {:?}", m.repositories[0].saved));
        assert_eq!(
            fetched_tree(&remote, &worktree.commit, "tracked.txt"),
            "local uncommitted content"
        );
        assert_eq!(
            fetched_tree(&remote, &worktree.commit, "untracked.txt"),
            "untracked payload"
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main"
            ]),
            original_main
        );
        verify_repository(&mut m.repositories[0], &remote_s);
        assert_eq!(m.repositories[0].verification, "isolated-verified");
        preview(&mut m, false);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        cleanup_repository(&m.repositories[0], &remote_s, &state, false).unwrap();
        assert!(!local.exists());
    }

    #[test]
    fn pushes_local_main_to_recovery_ref_then_deletes_only_fixture_clone() {
        let (_t, local, remote, remote_s) = fixture();
        let original_main = cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(
            m.repositories[0].preservation, "complete",
            "{:?}",
            m.repositories[0].verification_error
        );
        let pushed = m.repositories[0]
            .saved
            .iter()
            .find(|s| s.name == "branch:main")
            .unwrap();
        assert_eq!(pushed.verification, "push-succeeded");
        assert!(pushed.remote_ref.starts_with("recovery/"));
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main"
            ]),
            original_main,
            "remote main must remain unchanged"
        );
        let remote_oid = cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            &format!("refs/heads/{}", pushed.remote_ref),
        ]);
        assert_eq!(remote_oid, pushed.commit);
        verify_repository(&mut m.repositories[0], &remote_s);
        assert_eq!(m.repositories[0].verification, "isolated-verified");
        preview(&mut m, false);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        let removed = cleanup_repository(&m.repositories[0], &remote_s, &state, false).unwrap();
        assert_eq!(removed, vec![local.to_string_lossy().into_owned()]);
        assert!(!local.exists());
    }

    #[test]
    fn bare_cleanup_blocks_new_unreachable_object() {
        let (_temp, _local, remote, remote_s) = fixture();
        let bare = remote.parent().unwrap().join("local-bare.git");
        cmd(&[
            "git",
            "clone",
            "--bare",
            remote.to_str().unwrap(),
            bare.to_str().unwrap(),
        ]);
        let mut m = manifest(&bare, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "complete");
        verify_repository(&mut m.repositories[0], &remote_s);
        preview(&mut m, false);
        assert_eq!(m.repositories[0].deletion, "eligible");

        cmd(&[
            "git",
            "--git-dir",
            bare.to_str().unwrap(),
            "hash-object",
            "-w",
            "--stdin",
        ]);
        let state = bare.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        let error = cleanup_repository(&m.repositories[0], &remote_s, &state, false).unwrap_err();
        assert!(
            error.contains("new unpreserved unreachable Git object"),
            "{error}"
        );
        assert!(
            bare.exists(),
            "bare repository with a new object was deleted"
        );
    }

    #[test]
    fn isolated_verification_rejects_a_missing_preservation_ref() {
        let (_t, local, remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        let saved = m.repositories[0]
            .saved
            .iter()
            .find(|saved| saved.source == "branch")
            .unwrap()
            .clone();
        isolated_verify_saved(&remote_s, &saved).unwrap();
        cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "update-ref",
            "-d",
            &format!("refs/heads/{}", saved.remote_ref),
        ]);
        assert!(isolated_verify_saved(&remote_s, &saved).is_err());
    }

    #[test]
    fn saved_commit_requires_its_exact_remote_ref() {
        let (_temp, local, remote, remote_s) = fixture();
        let commit = cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", "HEAD"]);
        cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "update-ref",
            "refs/heads/other",
            &commit,
        ]);
        cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "update-ref",
            "-d",
            "refs/heads/main",
        ]);
        let saved = Saved {
            remote_ref: "main".into(),
            commit,
            ..Default::default()
        };

        assert!(!saved_commit_is_preserved(&remote_s, &saved).unwrap());
    }

    #[test]
    fn gitleaks_finding_blocks_all_uploads() {
        let (_t, local, remote, remote_s) = fixture();
        fs::write(
            local.join("secret.txt"),
            "github_pat_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\n",
        )
        .unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "secret.txt"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit",
            "-m",
            "fixture secret",
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "blocked");
        assert!(
            m.repositories[0]
                .saved
                .iter()
                .all(|saved| saved.verification == "pending-push")
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/recovery"
            ]),
            ""
        );
    }

    #[test]
    fn branches_only_secret_finding_blocks_every_branch_push() {
        let (_t, local, remote, remote_s) = fixture();
        fs::write(
            local.join("secret.txt"),
            "github_pat_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\n",
        )
        .unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "secret.txt"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit",
            "-m",
            "branch-only secret fixture",
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve_branches_only(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "blocked");
        assert!(m.repositories[0].saved.is_empty());
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/recovery"
            ]),
            ""
        );
    }

    #[test]
    fn lfs_payload_inventory_blocks_preservation_before_any_upload() {
        let (_t, local, remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        m.repositories[0].lfs_files.push("oid size path".into());
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "blocked");
        assert!(
            m.repositories[0]
                .verification_error
                .as_deref()
                .unwrap()
                .contains("unsupported Git LFS object ID")
        );
        assert!(
            m.repositories[0]
                .saved
                .iter()
                .all(|saved| saved.verification == "pending-push")
        );
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/recovery"
            ]),
            ""
        );
    }

    #[test]
    fn cleanup_blocks_unreachable_noncommit_without_a_saved_commit_tree() {
        let (_temp, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        verify_repository(&mut m.repositories[0], &remote_s);
        let orphan_blob = "0123456789abcdef0123456789abcdef01234567";
        m.repositories[0]
            .unreachable_noncommits
            .push(orphan_blob.into());

        assert_eq!(
            cleanup_blocker(&m.repositories[0], false).as_deref(),
            Some("blocked-unpreserved-unreachable-object:0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn ignored_worktree_content_blocks_full_preservation() {
        let (_temp, local, _remote, remote_s) = fixture();
        fs::write(local.join(".gitignore"), "cache/\n").unwrap();
        fs::create_dir_all(local.join("cache")).unwrap();
        fs::write(local.join("cache/ignored.bin"), b"local-only bytes").unwrap();
        let mut m = manifest(&local, &remote_s);

        preserve(&mut m).unwrap();
        preview(&mut m, false);

        assert_eq!(m.repositories[0].preservation, "blocked");
        assert!(
            m.repositories[0].worktrees[0]
                .ignored
                .iter()
                .any(|path| path.contains("cache"))
        );
        assert!(m.repositories[0].deletion.starts_with("blocked-"));
        assert!(local.join("cache/ignored.bin").exists());
    }

    #[test]
    fn detached_worktree_head_is_pushed_when_no_local_branch_names_it() {
        let (_t, local, remote, remote_s) = fixture();
        let linked = local.parent().unwrap().join("detached-wt");
        let tree = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "rev-parse",
            "HEAD^{tree}",
        ]);
        let detached = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            &cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", "HEAD"]),
            "-m",
            "detached-only",
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            &detached,
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(
            m.repositories[0].preservation, "complete",
            "{:?}",
            m.repositories[0].verification_error
        );
        let saved = m.repositories[0]
            .saved
            .iter()
            .find(|s| s.source == "detached")
            .unwrap();
        assert_eq!(saved.commit, detached);
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                &format!("refs/heads/{}", saved.remote_ref)
            ]),
            detached
        );
        assert_eq!(
            m.repositories[0]
                .saved
                .iter()
                .filter(|s| s.source == "branch")
                .count(),
            1
        );
    }

    #[test]
    fn deletes_only_after_all_local_branches_push_even_with_dirty_worktree() {
        let (_t, local, _remote, remote_s) = fixture();
        let linked = local.parent().unwrap().join("linked-feature");
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "feature",
            linked.to_str().unwrap(),
        ]);
        fs::write(linked.join("tracked.txt"), "feature working content\n").unwrap();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "complete");
        verify_repository(&mut m.repositories[0], &remote_s);
        assert_eq!(m.repositories[0].verification, "isolated-verified");
        preview(&mut m, false);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        let linked_canon = fs::canonicalize(&linked).unwrap();
        let removed = cleanup_repository(&m.repositories[0], &remote_s, &state, false).unwrap();
        assert!(removed.iter().any(|p| Path::new(p) == linked_canon));
        assert!(!linked.exists(), "linked worktree still exists");
        assert!(removed.iter().any(|p| p == local.to_str().unwrap()));
        assert!(!local.exists());
        assert!(!linked.exists());
    }

    #[test]
    fn branch_only_cleanup_retains_dirty_and_tagged_clone_after_branch_pushes() {
        let (_t, local, remote, remote_s) = fixture();
        fs::write(
            local.join("local-secret.txt"),
            "github_pat_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\n",
        )
        .unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "tag", "local-only"]);
        let mut m = manifest(&local, &remote_s);
        preserve_branches_only(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "branches-pushed");
        assert_eq!(
            m.repositories[0]
                .saved
                .iter()
                .filter(|saved| saved.source == "branch")
                .count(),
            m.repositories[0].branches.len()
        );
        let branch_commit = m.repositories[0]
            .saved
            .iter()
            .find(|saved| saved.source == "branch")
            .unwrap()
            .commit
            .clone();
        preview(&mut m, true);
        assert!(m.repositories[0].deletion.starts_with("blocked-"));
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        assert!(cleanup_repository(&m.repositories[0], &remote_s, &state, true).is_err());
        assert!(local.exists(), "branch-pushed dirty clone was deleted");
        assert!(local.join("local-secret.txt").exists());
        assert!(git(&local, &["show-ref", "--verify", "refs/tags/local-only"]).is_ok());
        assert!(
            !cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "ls-tree",
                "-r",
                "--name-only",
                &branch_commit,
            ])
            .lines()
            .any(|path| path == "local-secret.txt")
        );
    }

    #[test]
    fn branch_only_cleanup_blocks_local_tags_and_non_branch_refs() {
        let (_temp, local, _remote, remote_s) = fixture();
        let branch_oid = cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", "HEAD"]);
        cmd(&["git", "-C", local.to_str().unwrap(), "tag", "local-only"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "update-ref",
            "refs/notes/local-only",
            &branch_oid,
        ]);

        let mut m = manifest(&local, &remote_s);
        preserve_branches_only(&mut m).unwrap();
        preview(&mut m, true);
        assert!(
            m.repositories[0].deletion == "blocked-preservation",
            "branch-only preservation must not authorize deletion: {}",
            m.repositories[0].deletion
        );

        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        assert!(cleanup_repository(&m.repositories[0], &remote_s, &state, true).is_err());
        assert!(local.exists(), "clone with local-only refs was deleted");
        assert!(git(&local, &["show-ref", "--verify", "refs/tags/local-only"]).is_ok());
        assert!(git(&local, &["show-ref", "--verify", "refs/notes/local-only"]).is_ok());
    }

    #[test]
    fn full_cleanup_rechecks_worktree_immediately_before_delete() {
        let (_t, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        verify_repository(&mut m.repositories[0], &remote_s);
        preview(&mut m, false);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();

        fs::write(local.join("after-preview.txt"), "new untracked data\n").unwrap();
        let error = cleanup_repository(&m.repositories[0], &remote_s, &state, true).unwrap_err();
        assert!(
            error.contains("worktree changed since scan")
                || error.contains("worktree status changed")
                || error.contains("blocked-unpreserved-worktree-content"),
            "unexpected error: {error}"
        );
        assert!(local.exists(), "clone with new untracked data was deleted");
        assert!(local.join("after-preview.txt").exists());
    }

    #[test]
    fn cleanup_blocks_when_ignored_content_is_present() {
        let (_temp, local, _remote, remote_s) = fixture();
        fs::write(local.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(local.join("ignored.txt"), "ignored contents\n").unwrap();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "blocked");
        assert!(
            m.repositories[0]
                .verification_error
                .as_deref()
                .is_some_and(|error| error.contains("ignored content"))
        );
        preview(&mut m, false);
        assert!(m.repositories[0].deletion.starts_with("blocked-"));
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        let error = cleanup_repository(&m.repositories[0], &remote_s, &state, false).unwrap_err();
        assert!(
            error.contains("ignored-content") || error.contains("preservation"),
            "unexpected cleanup error: {error}"
        );
        assert!(local.exists(), "clone with ignored content was deleted");
        assert_eq!(
            fs::read_to_string(local.join("ignored.txt")).unwrap(),
            "ignored contents\n"
        );
    }

    #[test]
    fn unindexed_object_garbage_is_fingerprinted_and_blocks_cleanup() {
        let (_temp, local, _remote, remote_s) = fixture();
        let object_dir = PathBuf::from(common_dir(&local).unwrap()).join("objects");
        let pack = object_dir.join("tmp_pack_incomplete");
        let clean_inventory = inventory_one(&local, vec![], &remote_s);
        assert!(clean_inventory.object_garbage.is_empty());
        fs::write(&pack, b"opaque pack tail").unwrap();

        let inventory = inventory_one(&local, vec![], &remote_s);
        assert!(
            inventory.inventory_complete,
            "{:?}",
            inventory.inventory_errors
        );
        assert_eq!(inventory.object_garbage.len(), 1);
        let artifact = &inventory.object_garbage[0];
        assert_eq!(artifact.path, pack.to_string_lossy());
        assert_eq!(artifact.kind, "file");
        assert_eq!(artifact.size, 16);
        let expected_hash = hash_bytes(b"opaque pack tail");
        assert_eq!(artifact.sha256.as_deref(), Some(expected_hash.as_str()));
        assert!(
            cleanup_blocker(&inventory, false)
                .unwrap()
                .starts_with("blocked-unindexed-object-garbage:")
        );

        let err = cleanup_recheck_unreachable(&clean_inventory).unwrap_err();
        assert!(err.contains("unindexed Git object-directory garbage changed"));
        assert!(clean_inventory.object_garbage.is_empty());
        fs::remove_file(&pack).unwrap();
    }

    #[test]
    fn cleanup_rechecks_unreachable_objects_created_after_preservation() {
        let (_temp, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        verify_repository(&mut m.repositories[0], &remote_s);
        preview(&mut m, false);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();

        let tree = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "rev-parse",
            "HEAD^{tree}",
        ]);
        let parent = cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", "HEAD"]);
        let late = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit-tree",
            &tree,
            "-p",
            &parent,
            "-m",
            "late unreferenced commit",
        ]);
        let error = cleanup_repository(&m.repositories[0], &remote_s, &state, true).unwrap_err();
        assert!(error.contains(&late), "unexpected cleanup error: {error}");
        assert!(
            local.exists(),
            "clone with late unreachable commit was deleted"
        );
    }

    #[test]
    fn cleanup_requires_matching_isolated_verified_worktree_snapshot() {
        let (_temp, local, _remote, remote_s) = fixture();
        fs::write(local.join("untracked.txt"), "preserve me\n").unwrap();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        verify_repository(&mut m.repositories[0], &remote_s);
        let repository = &mut m.repositories[0];
        let path = repository.worktrees[0].path.clone();
        let expected_tree = repository.worktrees[0].worktree_tree.clone().unwrap();
        let snapshot = repository
            .saved
            .iter()
            .position(|saved| {
                saved.source == "worktree-snapshot"
                    && saved.name == format!("worktree:{path}")
                    && saved.tree.as_deref() == Some(expected_tree.as_str())
                    && saved.verification == "isolated-verified"
            })
            .unwrap();

        repository.saved.remove(snapshot);
        assert_eq!(
            cleanup_blocker(repository, false).as_deref(),
            Some(format!("blocked-unverified-worktree-snapshot:{path}").as_str())
        );
    }

    #[test]
    fn cleanup_blocks_tampered_worktree_snapshot_tree() {
        let (_temp, local, _remote, remote_s) = fixture();
        fs::write(local.join("untracked.txt"), "preserve me\n").unwrap();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        verify_repository(&mut m.repositories[0], &remote_s);
        let repository = &mut m.repositories[0];
        let path = repository.worktrees[0].path.clone();
        let snapshot = repository
            .saved
            .iter_mut()
            .find(|saved| {
                saved.source == "worktree-snapshot" && saved.name == format!("worktree:{path}")
            })
            .unwrap();
        snapshot.tree = Some("0".repeat(40));

        assert_eq!(
            cleanup_blocker(repository, false).as_deref(),
            Some(format!("blocked-unverified-worktree-snapshot:{path}").as_str())
        );
    }

    #[test]
    fn cleanup_requires_verified_staged_snapshot_when_index_differs_from_head() {
        let (_temp, local, _remote, remote_s) = fixture();
        fs::write(local.join("staged.txt"), "staged content\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "staged.txt"]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        verify_repository(&mut m.repositories[0], &remote_s);
        let repository = &mut m.repositories[0];
        let path = repository.worktrees[0].path.clone();
        assert!(repository.saved.iter().any(|saved| {
            saved.source == "staged-snapshot"
                && saved.name == format!("staged:{path}")
                && saved.tree == repository.worktrees[0].index_tree
                && saved.verification == "isolated-verified"
        }));
        repository
            .saved
            .retain(|saved| saved.source != "staged-snapshot");

        assert_eq!(
            cleanup_blocker(repository, false).as_deref(),
            Some(format!("blocked-unverified-staged-snapshot:{path}").as_str())
        );
    }

    #[test]
    fn cleanup_refuses_changed_branch_after_push() {
        let (_t, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        preview(&mut m, false);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "checkout",
            "--orphan",
            "late-branch",
        ]);
        fs::write(local.join("late.txt"), "late branch\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "late.txt"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit",
            "-m",
            "late branch",
        ]);
        let state = local.parent().unwrap().join("external-state");
        assert!(cleanup_repository(&m.repositories[0], &remote_s, &state, false).is_err());
        assert!(local.exists());
    }

    #[test]
    fn unsupported_tag_blocks_cleanup_but_does_not_skip_branch_pushes() {
        let (_temp, local, remote, remote_url) = fixture();
        cmd(&["git", "-C", local.to_str().unwrap(), "tag", "local-only"]);
        let mut manifest = manifest(&local, &remote_url);
        preserve(&mut manifest).unwrap();
        assert_eq!(manifest.repositories[0].preservation, "blocked");
        assert!(
            manifest.repositories[0]
                .verification_error
                .as_deref()
                .unwrap()
                .contains("refs/tags/")
        );
        let branch = manifest.repositories[0]
            .saved
            .iter()
            .find(|saved| saved.source == "branch")
            .unwrap();
        assert_eq!(
            remote_oid(&remote_url, &format!("refs/heads/{}", branch.remote_ref))
                .unwrap()
                .as_deref(),
            Some(branch.commit.as_str())
        );
        assert!(local.exists());
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "for-each-ref",
                "--format=%(refname)",
                "refs/tags",
            ]),
            ""
        );
    }

    #[test]
    fn exact_existing_annotated_tag_is_isolated_verified_without_mutating_remote() {
        let (_temp, local, remote, remote_url) = fixture();
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "tag",
            "-a",
            "local-only",
            "-m",
            "frozen tag",
            "HEAD",
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "push",
            "origin",
            "refs/tags/local-only:refs/tags/local-only",
        ]);
        let tag_ref = "refs/tags/local-only";
        let tag_oid = cmd(&["git", "-C", local.to_str().unwrap(), "rev-parse", tag_ref]);
        let remote_before = remote_oid(&remote_url, tag_ref).unwrap();
        assert_eq!(remote_before.as_deref(), Some(tag_oid.as_str()));

        let mut manifest = manifest(&local, &remote_url);
        preserve(&mut manifest).unwrap();
        let repository = &mut manifest.repositories[0];
        assert_ne!(
            repository.preservation, "blocked",
            "{:?}",
            repository.verification_error
        );
        assert_eq!(repository.existing_refs.len(), 1);
        assert_eq!(repository.existing_refs[0].reference, tag_ref);
        assert_eq!(repository.existing_refs[0].object, tag_oid);
        assert_eq!(repository.existing_refs[0].object_type, "tag");
        assert_eq!(
            repository.existing_refs[0].verification,
            "pending-isolated-verification"
        );

        verify_repository(repository, &remote_url);
        assert_eq!(
            repository.verification, "isolated-verified",
            "{:?}",
            repository.verification_error
        );
        assert_eq!(
            repository.existing_refs[0].verification,
            "isolated-verified"
        );
        assert_eq!(cleanup_blocker(repository, false), None);
        assert_eq!(remote_oid(&remote_url, tag_ref).unwrap(), remote_before);
        assert_eq!(
            cmd(&[
                "git",
                "--git-dir",
                remote.to_str().unwrap(),
                "cat-file",
                "-t",
                &tag_oid,
            ]),
            "tag"
        );
    }

    #[test]
    fn existing_tag_with_different_remote_object_still_blocks_cleanup() {
        let (_temp, local, _remote, remote_url) = fixture();
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "tag",
            "-a",
            "local-only",
            "-m",
            "remote tag object",
            "HEAD",
        ]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "push",
            "origin",
            "refs/tags/local-only:refs/tags/local-only",
        ]);
        let remote_tag_oid = remote_oid(&remote_url, "refs/tags/local-only")
            .unwrap()
            .unwrap();
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "tag",
            "-f",
            "-a",
            "local-only",
            "-m",
            "different local tag object",
            "HEAD",
        ]);
        let local_tag_oid = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "rev-parse",
            "refs/tags/local-only",
        ]);
        assert_ne!(local_tag_oid, remote_tag_oid);

        let mut manifest = manifest(&local, &remote_url);
        preserve(&mut manifest).unwrap();
        let repository = &manifest.repositories[0];
        assert_eq!(repository.preservation, "blocked");
        assert!(
            repository.verification_error.as_deref().is_some_and(
                |error| error.contains("unsupported local ref is missing or differs remotely")
            )
        );
        assert!(repository.existing_refs.is_empty());
        assert_eq!(
            remote_oid(&remote_url, "refs/tags/local-only")
                .unwrap()
                .as_deref(),
            Some(remote_tag_oid.as_str())
        );
    }

    #[test]
    fn refresh_updates_changed_branches_without_losing_previous_push_records() {
        let (_temp, local, _remote, remote_url) = fixture();
        let mut m = manifest(&local, &remote_url);
        preserve(&mut m).unwrap();
        let old_ref = m.repositories[0]
            .saved
            .iter()
            .find(|saved| saved.source == "branch")
            .unwrap()
            .remote_ref
            .clone();
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "switch",
            "-c",
            "later",
        ]);
        fs::write(local.join("later.txt"), "new branch\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "later.txt"]);
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "commit",
            "-m",
            "later branch",
        ]);
        refresh_repository(&mut m, &local).unwrap();
        assert!(
            m.repositories[0]
                .branches
                .iter()
                .any(|branch| branch.name == "later")
        );
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "complete");
        assert!(
            m.repositories[0]
                .saved
                .iter()
                .any(|saved| saved.remote_ref == old_ref)
        );
        assert!(
            m.repositories[0]
                .saved
                .iter()
                .any(|saved| saved.name == "branch:later")
        );
    }

    #[test]
    fn branch_inventory_keeps_exact_names_when_tags_are_ambiguous() {
        let (_temp, local, _remote, _remote_url) = fixture();
        cmd(&["git", "-C", local.to_str().unwrap(), "tag", "main"]);
        let names: Vec<_> = branch_inventory(&local)
            .unwrap()
            .into_iter()
            .map(|branch| branch.name)
            .collect();
        assert_eq!(names, ["main"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_scan_carries_saved_recovery_branch_ownership() {
        let mut previous = Manifest::default();
        previous.repositories.push(Repository {
            saved: vec![
                Saved {
                    source: "branch".into(),
                    remote_ref: "recovery/find-and-recovery/old/branch-a".into(),
                    commit: "a".repeat(40),
                    created_by_this_run: false,
                    verification: "push-succeeded".into(),
                    ..Default::default()
                },
                Saved {
                    source: "branch".into(),
                    remote_ref: "topic/unowned".into(),
                    commit: "b".repeat(40),
                    ..Default::default()
                },
                Saved {
                    source: "worktree-snapshot".into(),
                    name: "worktree:/projects/clone".into(),
                    remote_ref: "recovery/find-and-recovery/old/worktree".into(),
                    commit: "c".repeat(40),
                    ..Default::default()
                },
                Saved {
                    source: "staged".into(),
                    name: "staged:/projects/clone".into(),
                    remote_ref: "recovery/find-and-recovery/old/staged".into(),
                    commit: "d".repeat(40),
                    ..Default::default()
                },
                Saved {
                    source: "stash".into(),
                    name: "stash:0".into(),
                    remote_ref: "recovery/find-and-recovery/old/stash".into(),
                    commit: "e".repeat(40),
                    ..Default::default()
                },
                Saved {
                    source: "unreachable".into(),
                    name: "unreachable:orphan".into(),
                    remote_ref: "recovery/find-and-recovery/old/unreachable".into(),
                    commit: "f".repeat(40),
                    ..Default::default()
                },
                Saved {
                    source: "local-tag-objects".into(),
                    name: "tag:v-local".into(),
                    remote_ref: "recovery/find-and-recovery/old/tag".into(),
                    commit: "1".repeat(40),
                    ..Default::default()
                },
                Saved {
                    source: "lfs".into(),
                    name: "lfs:payload".into(),
                    remote_ref: "recovery/find-and-recovery/old/lfs".into(),
                    commit: "2".repeat(40),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let mut fresh_scan = Manifest::default();

        merge_recovery_ownership(&mut fresh_scan, &previous);

        assert_eq!(fresh_scan.recovery_ownership.len(), 7);
        assert_eq!(
            fresh_scan.recovery_ownership[0].remote_ref,
            "recovery/find-and-recovery/old/branch-a"
        );
        assert!(!fresh_scan.recovery_ownership[0].created_by_this_run);
        for source in [
            "branch",
            "worktree-snapshot",
            "staged",
            "stash",
            "unreachable",
            "local-tag-objects",
            "lfs",
        ] {
            assert!(
                fresh_scan
                    .recovery_ownership
                    .iter()
                    .any(|saved| saved.source == source)
            );
        }
        assert!(
            !fresh_scan
                .recovery_ownership
                .iter()
                .any(|saved| saved.remote_ref == "topic/unowned")
        );
    }

    #[test]
    fn fresh_scan_carries_deleted_copy_snapshot_mapping() {
        let path = "/projects/removed-clone".to_owned();
        let recovery = Saved {
            source: "branch".into(),
            name: "branch:main".into(),
            commit: "a".repeat(40),
            remote_ref: "recovery/find-and-recovery/b9064771/branch-main".into(),
            verification: "push-succeeded".into(),
            ..Default::default()
        };
        let existing_ref = ExistingRef {
            reference: "refs/tags/visual-baseline".into(),
            object: "b".repeat(40),
            object_type: "tag".into(),
            verification: "isolated-verified".into(),
        };
        let previous = Manifest {
            generated_unix: 123,
            deleted: vec![path.clone()],
            repositories: vec![Repository {
                path: path.clone(),
                common_dir: format!("{path}/.git"),
                branches: vec![Branch {
                    name: "main".into(),
                    commit: recovery.commit.clone(),
                }],
                saved: vec![recovery.clone()],
                existing_refs: vec![existing_ref.clone()],
                deletion: "deleted".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut fresh_scan = Manifest::default();

        merge_deletion_history(&mut fresh_scan, &previous);

        assert_eq!(fresh_scan.deleted, [path.as_str()]);
        assert_eq!(fresh_scan.deletion_history.len(), 1);
        let record = &fresh_scan.deletion_history[0];
        assert_eq!(record.local_path, path);
        assert_eq!(record.local_branches[0].name, "main");
        assert_eq!(record.local_branches[0].commit, recovery.commit);
        assert_eq!(record.snapshots[0].remote_ref, recovery.remote_ref);
        assert_eq!(record.snapshots[0].commit, recovery.commit);
        assert_eq!(record.existing_refs, [existing_ref]);
        assert_eq!(record.removed_paths, ["/projects/removed-clone"]);
        assert_eq!(record.completed_unix, 123);
    }

    #[test]
    fn recovered_deletion_records_remote_evidence_without_claiming_local_verification() {
        let remote_root = tempfile::tempdir().unwrap();
        let remote = remote_root.path().join("remote.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(&remote)
                .status()
                .unwrap()
                .success()
        );
        let source = repo();
        let oid = commit(source.path(), b"preserved payload\n");
        let common_dir = source.path().join(".git").to_string_lossy().into_owned();
        let prefix = hash_name(&common_dir);
        let reference = format!("refs/heads/recovery/find-and-recovery/{prefix}/branch-1234");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(source.path())
                .args([
                    "push",
                    remote.to_str().unwrap(),
                    &format!("{oid}:{reference}")
                ])
                .status()
                .unwrap()
                .success()
        );

        let missing = remote_root.path().join("deleted-clone");
        let before = Command::new("git")
            .args(["ls-remote", "--heads", remote.to_str().unwrap()])
            .output()
            .unwrap()
            .stdout;
        let mut manifest = Manifest {
            remote: remote.to_string_lossy().into_owned(),
            deletion_history: vec![DeletionRecord {
                local_path: missing.to_string_lossy().into_owned(),
                common_dir,
                ..Default::default()
            }],
            ..Default::default()
        };
        manifest.recovery_ownership.push(Saved {
            source: "branch".into(),
            name: "branch:main".into(),
            commit: oid.clone(),
            remote_ref: reference.strip_prefix("refs/heads/").unwrap().into(),
            ..Default::default()
        });
        record_recovered_deletion(&mut manifest, &missing, &prefix).unwrap();
        let record = manifest.unverified_deletion_history.last().unwrap();
        assert!(record.reconstructed);
        assert!(!record.local_state_verified);
        assert_eq!(record.local_branches[0].name, "main");
        assert_eq!(record.snapshots[0].commit, oid);
        assert!(record.evidence.contains("read-only-git-ls-remote"));
        assert!(manifest.deleted.is_empty());

        let history_len = manifest.unverified_deletion_history.len();
        record_recovered_deletion(&mut manifest, &missing, &prefix).unwrap();
        assert_eq!(manifest.unverified_deletion_history.len(), history_len);

        let arbitrary = remote_root.path().join("unrelated-absent-path");
        assert!(record_recovered_deletion(&mut manifest, &arbitrary, &prefix).is_err());
        assert!(record_recovered_deletion(&mut manifest, &missing, "not-a-hash").is_err());
        assert!(record_recovered_deletion(&mut manifest, &missing, "deadbeef").is_err());

        let no_refs_path = remote_root.path().join("known-but-no-refs");
        let no_refs_common_dir = no_refs_path.join(".git").to_string_lossy().into_owned();
        let no_refs_prefix = hash_name(&no_refs_common_dir);
        manifest.deletion_history.push(DeletionRecord {
            local_path: no_refs_path.to_string_lossy().into_owned(),
            common_dir: no_refs_common_dir,
            ..Default::default()
        });
        assert!(record_recovered_deletion(&mut manifest, &no_refs_path, &no_refs_prefix).is_err());

        let existing = source.path().join("still-here");
        fs::create_dir(&existing).unwrap();
        manifest.deletion_history.push(DeletionRecord {
            local_path: existing.to_string_lossy().into_owned(),
            common_dir: existing.join(".git").to_string_lossy().into_owned(),
            ..Default::default()
        });
        let existing_prefix = hash_name(&existing.join(".git").to_string_lossy());
        assert!(record_recovered_deletion(&mut manifest, &existing, &existing_prefix).is_err());

        let after = Command::new("git")
            .args(["ls-remote", "--heads", remote.to_str().unwrap()])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(before, after, "reconstruction must not mutate remote refs");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        for a in [
            vec!["git", "init", "-q"],
            vec!["git", "config", "user.name", "Recovery Test"],
            vec!["git", "config", "user.email", "recovery-test@localhost"],
        ] {
            assert!(
                Command::new(a[0])
                    .args(&a[1..])
                    .current_dir(p)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        d
    }

    #[test]
    fn worktree_path_presence_distinguishes_missing_from_present() {
        let root = tempfile::tempdir().unwrap();
        let present = root.path().join("present");
        fs::create_dir(&present).unwrap();
        assert!(worktree_path_exists(&present).unwrap());
        assert!(!worktree_path_exists(&root.path().join("absent")).unwrap());
    }

    #[test]
    fn registered_worktree_metadata_must_match_gitdir_and_common_directory() {
        let owner = repo();
        let common = owner.path().canonicalize().unwrap();
        let admin = common.join("worktrees").join("registered");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        let linked = owner.path().join("missing-linked");
        let linked_gitdir = linked.join(".git");
        assert_eq!(
            validate_registered_gitdir(&admin, &linked_gitdir).unwrap(),
            linked
        );
        validate_registered_commondir(&admin, &common).unwrap();

        assert!(validate_registered_gitdir(&admin, Path::new("relative/.git")).is_err());
        assert!(validate_registered_gitdir(&admin, &common.join("wrong/.git/entry")).is_err());
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(admin.join("commondir"), elsewhere.path().to_str().unwrap()).unwrap();
        assert!(validate_registered_commondir(&admin, &common).is_err());
        fs::write(admin.join("commondir"), "../..\n").unwrap();
    }

    #[test]
    fn registered_worktree_head_rejects_malformed_and_nonbranch_refs() {
        let owner = repo();
        assert!(parse_registered_worktree_head(owner.path(), "ref: refs/tags/release\n").is_err());
        assert!(
            parse_registered_worktree_head(owner.path(), "ref: refs/heads/bad..name\n").is_err()
        );
        assert!(parse_registered_worktree_head(owner.path(), "not-an-object-id\n").is_err());

        let (head, branch, detached) =
            parse_registered_worktree_head(owner.path(), "ref: refs/heads/unborn\n").unwrap();
        assert_eq!(head, None);
        assert_eq!(branch.as_deref(), Some("unborn"));
        assert!(!detached);
    }

    fn commit(p: &Path, bytes: &[u8]) -> String {
        fs::write(p.join("sample.txt"), bytes).unwrap();
        for a in [
            vec!["git", "add", "."],
            vec!["git", "commit", "-qm", "snapshot"],
        ] {
            assert!(
                Command::new(a[0])
                    .args(&a[1..])
                    .current_dir(p)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        git(p, &["rev-parse", "HEAD"]).unwrap().trim().into()
    }
    #[test]
    fn configured_core_bare_distinguishes_clone_storage_from_bare_repo() {
        let clone = repo();
        let bare_root = tempfile::tempdir().unwrap();
        let bare = bare_root.path().join("actual-bare.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(&bare)
                .status()
                .unwrap()
                .success()
        );

        assert!(!is_bare_git_directory(clone.path()));
        // Running rev-parse from inside a clone's .git directory may report
        // true; config is the stable distinction we use for deletion roots.
        assert!(!is_bare_git_directory(&clone.path().join(".git")));
        assert!(is_bare_git_directory(&bare));
    }

    #[test]
    fn remote_spellings_normalize_to_same_repository() {
        assert_eq!(
            canon_url("git@github.com:tailrocks/parallax.git"),
            "https://github.com/tailrocks/parallax"
        );
        assert_eq!(
            canon_url("ssh://git@github.com/tailrocks/parallax"),
            "https://github.com/tailrocks/parallax"
        );
        assert_eq!(
            canon_url("https://GITHUB.com:443/TailRocks/Parallax.git"),
            "https://github.com/tailrocks/parallax"
        );
    }

    #[test]
    fn local_and_generic_remote_paths_keep_case_and_git_suffix() {
        assert_ne!(canon_url("/tmp/Repo"), canon_url("/tmp/repo"));
        assert_ne!(
            canon_url("file:///tmp/repo.git"),
            canon_url("file:///tmp/repo")
        );
        assert_ne!(
            canon_url("https://git.example/Owner/Repo"),
            canon_url("https://git.example/owner/repo")
        );
    }
    #[test]
    fn secret_scan_accepts_safe_commit() {
        let d = repo();
        let oid = commit(d.path(), b"ordinary source\n");
        scan_commit(d.path(), &oid, "").unwrap();
    }

    #[test]
    fn secret_scan_drains_large_cat_file_batch_without_pipe_deadlock() {
        let d = repo();
        for i in 0..7000 {
            fs::write(d.path().join(format!("object-{i:05}.txt")), b"safe\n").unwrap();
        }
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(d.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "large batch"])
                .current_dir(d.path())
                .status()
                .unwrap()
                .success()
        );
        let oid = git(d.path(), &["rev-parse", "HEAD"]).unwrap();
        scan_commit(d.path(), oid.trim(), "").unwrap();
    }
    #[test]
    fn secret_scan_accepts_generic_source_variable() {
        let d = repo();
        let oid = commit(d.path(), b"let secret = \"abcdefghijklmnopqr\";\n");
        scan_commit(d.path(), &oid, "").unwrap();
    }
    #[test]
    fn secret_scan_blocks_canary_commit() {
        let d = repo();
        let oid = commit(
            d.path(),
            b"github_token=ghp_abcdefghijklmnopqrstuvwxyz1234567890\n",
        );
        assert!(scan_commit(d.path(), &oid, "").is_err());
    }

    #[test]
    fn temporary_recovery_fixtures_are_retained_without_upload() {
        let root = tempfile::tempdir().unwrap();
        let fixture = root.path().join("recover-smoke-test").join("repo");
        fs::create_dir_all(&fixture).unwrap();
        assert!(temporary_recovery_fixture(&fixture));
    }

    #[test]
    fn preview_keeps_deleted_path_state_after_directory_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let path = root
            .path()
            .join("removed-clone")
            .to_string_lossy()
            .into_owned();
        let mut manifest = Manifest {
            deleted: vec![path.clone()],
            repositories: vec![Repository {
                path: path.clone(),
                deletion: "deleted".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        preview(&mut manifest, false);
        assert_eq!(manifest.repositories[0].deletion, "deleted");
    }
}

#[cfg(test)]
mod dedupe_integration_tests {
    use super::*;

    fn run(args: &[String]) -> String {
        let output = Command::new(&args[0]).args(&args[1..]).output().unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn dedupe_execute_is_disabled_without_mutating_remote() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let local = temp.path().join("local");
        run(&[
            "git".into(),
            "init".into(),
            "--bare".into(),
            "-q".into(),
            remote.display().to_string(),
        ]);
        run(&[
            "git".into(),
            "init".into(),
            "-b".into(),
            "main".into(),
            "-q".into(),
            local.display().to_string(),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "config".into(),
            "user.name".into(),
            "Dedupe Test".into(),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "config".into(),
            "user.email".into(),
            "dedupe-test@localhost".into(),
        ]);
        fs::write(local.join("file.txt"), "fixture\n").unwrap();
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "add".into(),
            "file.txt".into(),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "commit".into(),
            "-m".into(),
            "fixture".into(),
        ]);
        let oid = run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "rev-parse".into(),
            "HEAD".into(),
        ]);
        let remote_s = remote.display().to_string();
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "push".into(),
            remote_s.clone(),
            "HEAD:refs/heads/main".into(),
        ]);
        for name in ["master", "stable", "unmanaged/alias"] {
            run(&[
                "git".into(),
                "-C".into(),
                local.display().to_string(),
                "push".into(),
                remote_s.clone(),
                format!("{oid}:refs/heads/{name}"),
            ]);
        }
        run(&[
            "git".into(),
            "--git-dir".into(),
            remote.display().to_string(),
            "symbolic-ref".into(),
            "HEAD".into(),
            "refs/heads/stable".into(),
        ]);

        let refs = [
            "recovery/find-and-recovery/integration/one",
            "recovery/find-and-recovery/integration/two",
        ];
        for name in refs {
            run(&[
                "git".into(),
                "-C".into(),
                local.display().to_string(),
                "push".into(),
                remote_s.clone(),
                format!("{oid}:refs/heads/{name}"),
            ]);
        }
        fs::write(local.join("same-tree.txt"), "same committed content\n").unwrap();
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "add".into(),
            "same-tree.txt".into(),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "commit".into(),
            "-m".into(),
            "same tree short history".into(),
        ]);
        let short_oid = run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "rev-parse".into(),
            "HEAD".into(),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "push".into(),
            remote_s.clone(),
            format!("{short_oid}:refs/heads/topic/short"),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "commit".into(),
            "--allow-empty".into(),
            "-m".into(),
            "same tree longer history".into(),
        ]);
        let long_oid = run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "rev-parse".into(),
            "HEAD".into(),
        ]);
        run(&[
            "git".into(),
            "-C".into(),
            local.display().to_string(),
            "push".into(),
            remote_s.clone(),
            format!("{long_oid}:refs/heads/topic/long"),
        ]);
        let mut manifest = Manifest {
            remote: remote_s.clone(),
            repositories: vec![Repository {
                deletion: "deleted".into(),
                saved: refs
                    .iter()
                    .map(|name| Saved {
                        source: "branch".into(),
                        commit: oid.clone(),
                        remote_ref: (*name).into(),
                        created_by_this_run: true,
                        verification: "push-succeeded".into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = temp.path().join("state");
        fs::create_dir_all(&state).unwrap();

        let error = dedupe(&mut manifest, &state, true).unwrap_err();
        assert!(error.contains("remote branch deletion is forbidden"));

        assert_eq!(
            remote_oid(&remote_s, "refs/heads/main").unwrap().as_deref(),
            Some(oid.as_str())
        );
        for branch in refs {
            assert_eq!(
                remote_oid(&remote_s, &format!("refs/heads/{branch}"))
                    .unwrap()
                    .as_deref(),
                Some(oid.as_str())
            );
        }
        for (branch, expected) in [
            ("master", oid.as_str()),
            ("stable", oid.as_str()),
            ("unmanaged/alias", oid.as_str()),
            ("topic/short", short_oid.as_str()),
            ("topic/long", long_oid.as_str()),
        ] {
            assert_eq!(
                remote_oid(&remote_s, &format!("refs/heads/{branch}"))
                    .unwrap()
                    .as_deref(),
                Some(expected),
                "unowned, protected, or different-OID branch {branch} changed"
            );
        }
        for oid in [oid.as_str(), short_oid.as_str()] {
            assert_eq!(
                remote_oid(
                    &remote_s,
                    &format!("refs/archive/find-and-recovery/dedupe/{oid}")
                )
                .unwrap(),
                None,
                "dedupe execute must not create archive refs"
            );
        }
    }

    #[test]
    fn rejects_matching_git_url_rewrites() {
        let repo = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(init.success());
        let config = Command::new("git")
            .args([
                "-C",
                repo.path().to_str().unwrap(),
                "config",
                "url.https://redirect.invalid/.insteadOf",
                "file:///safe/",
            ])
            .status()
            .unwrap();
        assert!(config.success());
        assert!(reject_remote_url_rewrite("file:///safe/remote.git", Some(repo.path())).is_err());
        assert!(reject_remote_url_rewrite("file:///other/remote.git", Some(repo.path())).is_ok());
    }
}
