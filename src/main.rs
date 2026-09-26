#[path = "lfs_batch.rs"]
mod lfs_batch;
#[path = "remote_lfs.rs"]
mod remote_lfs;
mod conditional_delete;
mod dedupe;
mod remote_snapshot;

use clap::{Parser, Subcommand};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs,
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
    #[arg(long, default_value = "/Users/donbeave/.local/share/find-and-recovery")]
    state: PathBuf,
    #[command(subcommand)]
    command: Phase,
}
#[derive(Subcommand)]
enum Phase {
    Scan {
        #[arg(long, value_delimiter = ',')]
        roots: Vec<PathBuf>,
        #[arg(long)]
        root_list: Option<PathBuf>,
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
    Dedupe {
        #[arg(long)]
        execute: bool,
    },
    ResumePartial {
        #[arg(long)]
        execute: bool,
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
    inventory_complete: bool,
    inventory_errors: Vec<String>,
    saved: Vec<Saved>,
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
    removed_paths: Vec<String>,
    completed_unix: u64,
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
    deleted: Vec<String>,
}

fn record_deleted_copy(manifest: &mut Manifest, repository: &Repository, removed_paths: &[String]) {
    let record = DeletionRecord {
        local_path: repository.path.clone(),
        common_dir: repository.common_dir.clone(),
        local_branches: repository.branches.clone(),
        snapshots: repository.saved.clone(),
        removed_paths: removed_paths.to_vec(),
        completed_unix: now(),
    };
    if !manifest.deletion_history.iter().any(|previous| {
        previous.local_path == record.local_path
            && previous.common_dir == record.common_dir
            && previous.snapshots == record.snapshots
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
    for record in &previous.deletion_history {
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
                record.local_path == repository.path && record.snapshots == repository.saved
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
            removed_paths: if removed_paths.is_empty() {
                vec![repository.path.clone()]
            } else {
                removed_paths
            },
            completed_unix: previous.generated_unix,
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    let mut s = u.trim().trim_end_matches('/').to_string();
    if let Some(x) = s.strip_prefix("git@github.com:") {
        s = format!("https://github.com/{x}")
    }
    if let Some(x) = s.strip_prefix("ssh://git@github.com/") {
        s = format!("https://github.com/{x}")
    }
    if s.ends_with(".git") {
        s.truncate(s.len() - 4)
    }
    s.to_lowercase()
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
        if origin_urls.iter().any(|url| canon_url(url) != canon_url(target)) {
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
        if remote_name.is_empty() || !upstream_ref.starts_with("refs/heads/") {
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
#[cfg(unix)]
fn filesystem_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}
#[cfg(not(unix))]
fn filesystem_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}
fn parse_worktrees(path: &Path) -> Result<Vec<Worktree>, String> {
    let text = match git(path, &["worktree", "list", "--porcelain"]) {
        Ok(text) => text,
        Err(e) if e.contains("Invalid path") && e.contains("No such file or directory") => {
            // Git refuses to list otherwise-valid worktrees when a stale
            // linked-worktree registration points below a removed parent.
            // Recover the primary tree and every admin HEAD from metadata.
            let top = PathBuf::from(git(path, &["rev-parse", "--show-toplevel"])?.trim());
            let head = git(&top, &["rev-parse", "HEAD"])?.trim().to_owned();
            let branch = git(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .ok()
                .map(|s| s.trim().to_owned());
            let mut rows = vec![Worktree {
                path: top.to_string_lossy().into_owned(),
                head: Some(head),
                branch: branch.clone(),
                detached: branch.is_none(),
                ..Default::default()
            }];
            let common = common_dir(path)?;
            let admin = common.join("worktrees");
            if admin.exists() {
                for entry in fs::read_dir(admin).map_err(|e| e.to_string())? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    let worktree_gitdir = fs::read_to_string(entry.path().join("gitdir"))
                        .map_err(|e| e.to_string())?;
                    let linked_gitdir = PathBuf::from(worktree_gitdir.trim());
                    let linked_path = linked_gitdir
                        .parent()
                        .ok_or("bad linked worktree gitdir")?
                        .to_path_buf();
                    let head_text =
                        fs::read_to_string(entry.path().join("HEAD")).map_err(|e| e.to_string())?;
                    let (head, branch) =
                        if let Some(reference) = head_text.trim().strip_prefix("ref: ") {
                            let branch = reference.strip_prefix("refs/heads/").map(str::to_owned);
                            let oid = git(path, &["rev-parse", reference])
                                .ok()
                                .map(|s| s.trim().to_owned());
                            (oid, branch)
                        } else {
                            (Some(head_text.trim().to_owned()), None)
                        };
                    let missing = !linked_path.exists();
                    rows.push(Worktree {
                        path: linked_path.to_string_lossy().into_owned(),
                        head,
                        detached: branch.is_none(),
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
                rows.push(Worktree {
                    path: p.clone(),
                    head: cur.get("HEAD").cloned(),
                    branch: cur
                        .get("branch")
                        .and_then(|x| x.strip_prefix("refs/heads/").map(str::to_owned)),
                    detached: cur.contains_key("detached"),
                    bare: cur.contains_key("bare"),
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
    if !p.exists() {
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
    wt.index_tree = git(&p, &["write-tree"]).ok().map(|x| x.trim().to_owned());
    let clean = wt.status.iter().all(|line| line.starts_with('#'))
        && wt.untracked.is_empty()
        && wt.ignored.is_empty();
    if clean {
        wt.worktree_tree = if let Some(head) = &wt.head {
            git(&p, &["rev-parse", &format!("{head}^{{tree}}")])
                .ok()
                .map(|tree| tree.trim().to_owned())
        } else {
            wt.index_tree.clone()
        };
    } else if wt.ignored.is_empty() {
        // Dirty worktrees need a complete content tree; clean worktrees reuse HEAD.
        if let Some(head) = &wt.head {
            let td = tempfile::tempdir().map_err(|e| e.to_string())?;
            let index = td.path().join("index");
            git_with_index(&p, &["read-tree", head], &index)?;
            if git_with_index(&p, &["add", "-A", "--", "."], &index).is_ok() {
                wt.worktree_tree = git_with_index(&p, &["write-tree"], &index)
                    .ok()
                    .map(|x| x.trim().to_owned());
            }
        }
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
        let root_skips = std::cell::RefCell::new(Vec::new());
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
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    if [
                        "node_modules",
                        "target",
                        "target-review",
                        "vendor",
                        "dist",
                        "build",
                        ".cache",
                        "Caches",
                        "DerivedData",
                        "Pods",
                        ".gradle",
                        ".m2",
                        ".rustup",
                        ".npm",
                        ".yarn",
                        ".velnor-store",
                        "registry",
                    ]
                    .iter()
                    .any(|excluded| name.eq_ignore_ascii_case(excluded))
                    {
                        root_skips.borrow_mut().push(format!(
                            "generated/cache directory not traversed: {}",
                            e.path().display()
                        ));
                        return false;
                    }
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
        gaps.extend(root_skips.into_inner());
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
    let uncached = {
        let cache = cache.lock().map_err(|e| e.to_string())?;
        oids.iter()
            .filter(|oid| !cache.contains_key(&format!("{repo_name}\0{remote}\0{oid}")))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    };
    if uncached.is_empty() {
        return Ok(());
    }
    let result = scan_commits_uncached(repo, &uncached, remote, &[]);
    let mut cache = cache.lock().map_err(|e| e.to_string())?;
    for oid in uncached {
        cache.insert(format!("{repo_name}\0{remote}\0{oid}"), result.clone());
    }
    result
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
    let report = env::temp_dir().join(format!(
        "far-gitleaks-{}-{}.json",
        std::process::id(),
        hash_name(&range)
    ));
    let scan = vec![
        "gitleaks".into(),
        "git".into(),
        "--no-banner".into(),
        "--redact".into(),
        "--log-opts".into(),
        range,
        "--report-format".into(),
        "json".into(),
        "--report-path".into(),
        report.to_string_lossy().into_owned(),
        repo.to_string_lossy().into_owned(),
    ];
    let scan_result = out(&scan, None, &[]);
    let findings = fs::read(&report).unwrap_or_default();
    let _ = fs::remove_file(&report);
    if scan_result.is_err() {
        return Err("gitleaks failed or found a secret; upload blocked".into());
    }
    // Gitleaks may omit an empty report file; successful exit with no report means no findings.
    let finding_count = if findings.is_empty() {
        0
    } else {
        serde_json::from_slice::<serde_json::Value>(&findings)
            .ok()
            .and_then(|v| v.as_array().map(|a| a.len()))
            .unwrap_or(usize::MAX)
    };
    if finding_count != 0 {
        return Err(format!(
            "gitleaks findings detected ({finding_count}); upload blocked"
        ));
    }
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
) -> Result<Vec<remote_lfs::LfsObject>, String> {
    let commits = saved.iter().map(|entry| entry.commit.clone()).collect::<Vec<_>>();
    let objects = remote_lfs::inventory_local_lfs_commits(repo, &commits)?;
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
fn remote_has_branch_tip(remote: &str, commit: &str) -> Result<bool, String> {
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
        .any(|line| line.split_whitespace().next() == Some(commit)))
}
fn saved_commit_is_preserved(remote: &str, saved: &Saved) -> Result<bool, String> {
    let reference = format!("refs/heads/{}", saved.remote_ref);
    if remote_oid(remote, &reference)?.as_deref() == Some(saved.commit.as_str()) {
        return Ok(true);
    }
    remote_has_branch_tip(remote, &saved.commit)
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
    let a = vec![
        "git".into(),
        "-c".into(),
        "http.version=HTTP/1.1".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "push".into(),
        "--porcelain".into(),
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
    for r in &mut m.repositories {
        if r.deletion == "deleted" && !Path::new(&r.path).exists() {
            continue;
        }
        let prior_saved = r.saved.clone();
        r.preservation = "blocked".into();
        r.saved.clear();
        r.preservation_errors.clear();
        r.lfs_objects.clear();
        r.lfs_preservation = "pending".into();
        r.verification_error = None;
        if temporary_recovery_fixture(Path::new(&r.path)) {
            r.verification_error =
                Some("ambiguous temporary recovery fixture; retained without upload".into());
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
        let unsupported = r.refs.iter().find_map(|entry| {
            let (name, tail) = entry.split_once(' ')?;
            let supported = name.starts_with("refs/heads/")
                || name.starts_with("refs/remotes/")
                || name == "refs/stash"
                || (name.starts_with("refs/recovery-local/")
                    && tail.split_whitespace().nth(1) == Some("commit"));
            (!supported).then_some(name)
        });
        let mut blocker = unsupported
            .map(|reference| format!("unsupported local ref blocks cleanup: {reference}"));
        let path = PathBuf::from(&r.path);
        let remote = m.remote.clone();
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
        let current_branches = branch_inventory(&path)?;
        let current_worktrees = parse_worktrees(&path)?;
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
        // Inspect every tip independently. One secret or corrupt object must
        // block its containing clone, but must not hide which other tips were
        // safely recoverable or prevent their preservation attempts.
        let mut failure = None;
        for oid in scan_oids {
            if let Err(error) = scan_commit(&path, &oid, &remote) {
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
            for entry in r.refs.clone() {
                let Some((name, tail)) = entry.split_once(' ') else {
                    continue;
                };
                if !name.starts_with("refs/remotes/") {
                    continue;
                }
                let oid = tail.split_whitespace().next().unwrap_or("");
                if mapped.contains(oid) {
                    continue;
                }
                match remote_has_branch_tip(&remote, oid) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        record_preservation_failure(
                            r,
                            &mut failure,
                            format!("check remote-tracking {name} {oid}"),
                            error,
                        );
                        continue;
                    }
                }
                if let Err(error) = save_object(
                    &remote,
                    &common,
                    r,
                    &path,
                    &format!("remote-tracking:{name}"),
                    oid,
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
        if let Some(error) = failure.clone().or(blocker) {
            r.preservation = "blocked".into();
            r.verification_error = Some(error);
        } else {
            match preserve_saved_lfs(&path, &remote, &r.saved) {
                Ok(objects) => {
                    r.lfs_objects = objects;
                    r.lfs_preservation = if r.lfs_objects.is_empty() {
                        "not-required".into()
                    } else {
                        "push-succeeded".into()
                    };
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
                                record_preservation_failure(
                                    r,
                                    &mut failure,
                                    format!("push saved {} {}", saved.remote_ref, saved.commit),
                                    error,
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    r.lfs_preservation = "blocked".into();
                    record_preservation_failure(
                        r,
                        &mut failure,
                        "preserve LFS payloads".into(),
                        error,
                    );
                }
            }
            if let Some(error) = failure {
                r.preservation = "blocked".into();
                r.verification_error = Some(error);
            }
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
    for r in &mut m.repositories {
        if r.deletion == "deleted" && !Path::new(&r.path).exists() {
            continue;
        }
        let prior_saved = r.saved.clone();
        r.preservation = "blocked".into();
        r.verification_error = None;
        r.preservation_errors.clear();
        if temporary_recovery_fixture(Path::new(&r.path)) {
            r.verification_error =
                Some("ambiguous temporary recovery fixture; retained without upload".into());
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
        verification: "push-succeeded".into(),
    });
    Ok(())
}

fn write_worktree_tree(repo: &Path, head: &str) -> Result<String, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let index = directory.path().join("index");
    let index_path = index.to_str().ok_or("non-UTF8 temporary index")?;
    let envs = [("GIT_INDEX_FILE", index_path)];
    let read = if head.is_empty() { "--empty" } else { head };
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "read-tree".into(),
        read.into(),
    ];
    out(&args, None, &envs)?;
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
    let args = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "write-tree".into(),
    ];
    Ok(out(&args, None, &envs)?.trim().into())
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
    let staged = git(path, &["write-tree"])?.trim().to_owned();
    let head_tree = if head.is_empty() {
        String::new()
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
    if out(&scan, None, &[]).is_err() {
        return Err(format!(
            "working-file secret scan blocked upload: {}",
            wt.path
        ));
    }
    let mut staged_commit = None;
    if staged != head_tree && staged != full {
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
            verification: "push-succeeded".into(),
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
        verification: "push-succeeded".into(),
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

fn verify_repository(r: &mut Repository, remote: &str) {
    r.verification = "blocked".into();
    r.verification_error = None;
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
    let refs = r
        .saved
        .iter()
        .map(|saved| format!("refs/heads/{}", saved.remote_ref))
        .collect::<Vec<_>>();
    let actual = match remote_lfs::verify_remote_lfs_refs(remote, &refs) {
        Ok(objects) => objects,
        Err(error) => {
            r.verification_error = Some(error);
            return;
        }
    };
    let expected = r
        .lfs_objects
        .iter()
        .map(|object| (object.oid.as_str(), object.size))
        .collect::<BTreeSet<_>>();
    let actual = actual
        .iter()
        .map(|object| (object.oid.as_str(), object.size))
        .collect::<BTreeSet<_>>();
    if actual != expected {
        r.verification_error = Some(format!(
            "remote LFS pointer set differs from saved local inventory (expected {}, fetched {})",
            expected.len(),
            actual.len()
        ));
        return;
    }
    for saved in &mut r.saved {
        saved.verification = "isolated-verified".into();
    }
    r.verification = "isolated-verified".into();
}

fn cleanup_blocker(r: &Repository, _branches_only: bool) -> Option<String> {
    if temporary_recovery_fixture(Path::new(&r.path)) {
        return Some("blocked-temporary-recovery-fixture".into());
    }
    if !r.inventory_complete {
        return Some("blocked-incomplete-inventory".into());
    }
    // Branch-only preservation is a push convenience, never deletion proof.
    // Any cleanup mode requires the full inventory and isolated verification.
    if r.preservation != "complete" {
        return Some("blocked-preservation".into());
    }
    if r.saved.iter().any(|saved| {
        saved.verification != "isolated-verified"
    }) {
        return Some("blocked-isolated-verification-incomplete".into());
    }
    if !matches!(r.lfs_preservation.as_str(), "not-required" | "push-succeeded") {
        return Some("blocked-LFS-preservation-incomplete".into());
    }
    for worktree in r
        .worktrees
        .iter()
        .filter(|w| !w.ignored.is_empty())
    {
        if !r.saved.iter().any(|saved| {
            saved.source == "worktree-snapshot"
                && saved.name == format!("worktree:{}", worktree.path)
                && saved.verification == "isolated-verified"
        }) {
            return Some(format!(
                "blocked-unpreserved-ignored-content:{}",
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
            } else if item == "<nested-repository scan skipped because ignored content blocks cleanup>"
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
            !r.saved.iter().any(|saved| saved.source == "stash" && saved.commit == oid)
        })
    {
        return Some("blocked-unpreserved-stash".into());
    }
    for worktree in &r.worktrees {
        if worktree.stashes.iter().any(|stash| {
            let oid = stash.split_whitespace().next().unwrap_or_default();
            !r.saved.iter().any(|saved| saved.source == "stash" && saved.commit == oid)
        }) {
            return Some(format!("blocked-unpreserved-worktree-stash:{}", worktree.path));
        }
    }
    for oid in &r.unreachable_commits {
        if !r.saved.iter().any(|saved| {
            git(Path::new(&r.path), &["merge-base", "--is-ancestor", oid, &saved.commit]).is_ok()
        }) {
            return Some(format!("blocked-unpreserved-unreachable-commit:{oid}"));
        }
    }
    if !r.unreachable_noncommits.is_empty() {
        let represented = r.saved.iter().flat_map(|saved| {
            git(Path::new(&r.path), &["rev-list", "--objects", &saved.commit])
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
                .collect::<Vec<_>>()
        }).collect::<BTreeSet<_>>();
        if let Some(oid) = r.unreachable_noncommits.iter().find(|oid| !represented.contains(*oid)) {
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
        let (Some(reference), Some(oid), Some(kind)) = (fields.next(), fields.next(), fields.next()) else {
            return Some(entry.clone());
        };
        if reference.starts_with("refs/heads/") {
            return None;
        }
        // Remote-tracking refs are cached views of the target remote. Local-only
        // tracking tips are separately inventoried and saved by preserve().
        if reference.starts_with("refs/remotes/") && kind == "commit"
            && r.saved.iter().any(|saved| {
                git(Path::new(&r.path), &["merge-base", "--is-ancestor", oid, &saved.commit]).is_ok()
            }) {
            return None;
        }
        if reference == "refs/stash" && kind == "commit"
            && r.saved.iter().any(|saved| saved.source == "stash" && saved.commit == oid) {
            return None;
        }
        // Tags, notes, replace refs, and unknown namespaces need their own
        // preservation format; never drop them with the local repository.
        Some(reference.to_owned())
    })
}
fn preview(m: &mut Manifest, branches_only: bool) {
    let deleted = m.deleted.iter().cloned().collect::<BTreeSet<_>>();
    for r in &mut m.repositories {
        if deleted.contains(&r.path) && !Path::new(&r.path).exists() {
            r.deletion = "deleted".into();
            continue;
        }
        r.deletion = cleanup_blocker(r, false).unwrap_or_else(|| "eligible".into());
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

fn branch_saved(r: &Repository) -> impl Iterator<Item = &Saved> {
    r.saved.iter().filter(|saved| saved.source == "branch")
}

fn current_ref_inventory(path: &Path) -> Result<Vec<String>, String> {
    Ok(git(
        path,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(objecttype)",
        ],
    )?
    .lines()
    .map(str::to_owned)
    .collect())
}

fn branches_only_worktree_blocker(worktree: &Worktree) -> Option<String> {
    if worktree.missing {
        return Some(format!("blocked-missing-worktree:{}", worktree.path));
    }
    if worktree.foreign_registration {
        return Some(format!("blocked-worktree-dependency:{}", worktree.path));
    }
    if worktree.detached {
        return Some(format!("blocked-unpreserved-detached-head:{}", worktree.path));
    }
    if !worktree.stashes.is_empty()
        || worktree.status.iter().any(|line| !line.starts_with('#'))
        || !worktree.untracked.is_empty()
        || !worktree.ignored.is_empty()
    {
        return Some(format!(
            "blocked-unpreserved-worktree-content:{}",
            worktree.path
        ));
    }
    for item in &worktree.nested_repositories {
        if let Some(path) = skipped_tree_path(item) {
            if ensure_no_nested_git(path).is_err() {
                return Some(format!("blocked-worktree-dependency:{}", worktree.path));
            }
        } else if item == "<nested-repository scan skipped because ignored content blocks cleanup>"
        {
            if validate_ignored_nested_inventory(worktree).is_err() {
                return Some(format!("blocked-worktree-dependency:{}", worktree.path));
            }
        } else {
            return Some(format!("blocked-worktree-dependency:{}", worktree.path));
        }
    }
    None
}

fn worktree_path_is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn recheck_branches_only_worktree(
    owner: &Path,
    common: &Path,
    expected: &Worktree,
    remote: &str,
) -> Result<(), String> {
    if expected.missing && worktree_path_is_absent(Path::new(&expected.path)) {
        return Ok(());
    }
    let mut current = expected.clone();
    inventory_worktree(owner, common, &mut current, remote)?;
    if let Some(reason) = branches_only_worktree_blocker(&current) {
        return Err(reason);
    }
    if current.fingerprint != expected.fingerprint
        || current.status != expected.status
        || current.untracked != expected.untracked
        || current.ignored != expected.ignored
        || current.stashes != expected.stashes
        || current.nested_repositories != expected.nested_repositories
    {
        return Err(format!("worktree changed since scan: {}", expected.path));
    }
    Ok(())
}

fn cleanup_recheck(r: &Repository, remote: &str) -> Result<(), String> {
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
    if filesystem_identity(&owner) != r.device.zip(r.inode) {
        return Err("repository device/inode changed since inventory".into());
    }
    if owner_canon == Path::new("/")
        || owner_canon == Path::new("/Users/donbeave")
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
        git(
            &owner,
            &[
                "worktree",
                "remove",
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
        if filesystem_identity(&owner) != r.device.zip(r.inode) {
            return Err("repository device/inode changed immediately before deletion".into());
        }
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
    if filesystem_identity(&owner) != r.device.zip(r.inode) {
        return Err("repository device/inode changed immediately before deletion".into());
    }
    fs::remove_dir_all(&owner)
        .map_err(|e| format!("remove exact clone {}: {e}", owner.display()))?;
    removed.push(owner.to_string_lossy().into_owned());
    Ok(removed)
}
fn dedupe(m: &mut Manifest, _state: &Path, execute: bool) -> Result<(), String> {
    if execute {
        return Err("dedupe --execute disabled: remote branch deletion is forbidden".into());
    }
    let remote = m.remote.clone();
    reject_remote_url_rewrite(&remote, None)?;
    let branches = remote_snapshot::fetch_remote_snapshots(&remote)?;
    let branch_facts = branches
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
    let protected = branch_facts
        .keys()
        .filter(|name| {
            !owned.contains(*name)
                || matches!(name.as_str(), "main" | "master")
                || name.as_str() == default_branch
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let groups = dedupe::exact_duplicate_groups(branches, &protected);
    for group in &groups {
        println!(
            "commit:{}\t{}\tkeep:{}\tdelete:{}",
            group.oid,
            group.branches.join(","),
            group.keeper,
            group.delete_candidates.join(",")
        );
        for name in &group.delete_candidates {
            let branch = branch_facts
                .get(name)
                .ok_or_else(|| format!("missing scanned snapshot for {name}"))?;
            let keeper = branch_facts
                .get(&group.keeper)
                .ok_or_else(|| format!("missing scanned keeper snapshot for {}", group.keeper))?;
            if branch.oid != group.oid || keeper.oid != group.oid {
                return Err(format!("candidate and keeper OIDs differ for {name}"));
            }
            println!(
                "preview-delete\t{name}\tkeep\t{}\tcommit\t{}",
                group.keeper, group.oid
            );
        }
    }
    Ok(())
}
fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let state = cli.state.canonicalize().unwrap_or(cli.state.clone());
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
        Phase::Scan { roots, root_list } => {
            let mut roots = roots;
            if let Some(list) = root_list {
                let contents = fs::read_to_string(&list)
                    .map_err(|e| format!("read root list {}: {e}", list.display()))?;
                roots.extend(
                    contents
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(PathBuf::from),
                );
            }
            let roots = if roots.is_empty() {
                vec![
                    "/Users".into(),
                    "/Volumes".into(),
                    "/opt".into(),
                    "/usr/local".into(),
                    "/tmp".into(),
                    "/private/tmp".into(),
                ]
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
            let (r, g) = discover(&roots, &remote, |partial, gaps| {
                m.repositories = partial.to_vec();
                m.coverage_gaps = gaps.to_vec();
                m.generated_unix = now();
                save(&m, &state)
            })?;
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
            println!("preservation recorded; inspect manifest")
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
                for i in 0..m.repositories.len() {
                    if m.repositories[i].deletion != "eligible" {
                        println!("{}\t{}", m.repositories[i].deletion, m.repositories[i].path);
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
                        }
                    }
                    save(&m, &state)?;
                }
                println!("cleanup recorded; inspect manifest");
            }
        }
        Phase::Dedupe { execute } => dedupe(&mut m, &state, execute)?,
        Phase::ResumePartial { execute } => {
            if m.repositories.len() != 1 {
                return Err("partial cleanup needs one exact manifest candidate".into());
            }
            let candidate = &m.repositories[0];
            let path = PathBuf::from(&candidate.path);
            if canon_url(&cli.remote) != canon_url(&m.remote)
                || candidate.saved.is_empty()
                || !candidate.inventory_complete
            {
                return Err("partial cleanup manifest is incomplete".into());
            }
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            let git_dir = path.join(".git");
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || !git_dir.is_dir()
                || git_dir.join("config").exists()
                || git_dir.join("HEAD").exists()
            {
                return Err("candidate is not the expected partially removed clone".into());
            }
            for branch in &candidate.branches {
                let reference = format!(
                    "refs/heads/recovery/parallax/48c59391f8d4/branch-{}",
                    branch.name
                );
                if remote_oid(&m.remote, &reference)?.as_deref() != Some(branch.commit.as_str()) {
                    return Err(format!(
                        "remote recovery ref missing or changed: {}",
                        branch.name
                    ));
                }
            }
            for saved in &candidate.saved {
                let reference = format!("refs/heads/{}", saved.remote_ref);
                if remote_oid(&m.remote, &reference)?.as_deref() != Some(saved.commit.as_str()) {
                    return Err(format!(
                        "remote recovery ref missing or changed: {}",
                        saved.remote_ref
                    ));
                }
            }
            println!(
                "partial cleanup candidate has matching branch and artifact refs: {}",
                path.display()
            );
            if execute {
                fs::remove_dir_all(&path)
                    .map_err(|e| format!("remove exact remaining path {}: {e}", path.display()))?;
                if path.exists() {
                    return Err("candidate still exists after cleanup".into());
                }
                m.repositories[0].deletion = "deleted-resumed-partial-cleanup".into();
                let repository = m.repositories[0].clone();
                record_deleted_copy(&mut m, &repository, &[path.to_string_lossy().into_owned()]);
                save(&m, &state)?;
                println!("deleted exact remaining path: {}", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod preservation_tests {
    use super::*;

    #[test]
    fn full_preserve_guard_rejects_target_origin_with_foreign_branch_upstream() {
        let (_temp, _remote, local, remote_s) = fixture();
        let foreign = local.parent().unwrap().join("foreign.git");
        cmd(&["git", "init", "--bare", foreign.to_str().unwrap()]);
        cmd(&[
            "git", "-C", local.to_str().unwrap(), "remote", "add", "jackin",
            foreign.to_str().unwrap(),
        ]);
        cmd(&[
            "git", "-C", local.to_str().unwrap(), "config", "branch.main.remote", "jackin",
        ]);
        cmd(&[
            "git", "-C", local.to_str().unwrap(), "config", "branch.main.merge", "refs/heads/main",
        ]);
        assert_eq!(branch_upstreams_target_remote(&local, &remote_s).unwrap(), false);
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
        let new_oid = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "rev-parse",
            "HEAD",
        ]);
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
        assert_eq!(m.repositories[0].preservation, "complete");
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
            m.repositories[0]
                .deletion
                .starts_with("blocked-non-branch-local-ref:"),
            "local-only refs were not blockers: {}",
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
    fn branch_only_cleanup_rechecks_clean_worktree_immediately_before_delete() {
        let (_t, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve_branches_only(&mut m).unwrap();
        preview(&mut m, true);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();

        fs::write(local.join("after-preview.txt"), "new untracked data\n").unwrap();
        let error = cleanup_repository(&m.repositories[0], &remote_s, &state, true).unwrap_err();
        assert!(
            error.contains("worktree changed since scan")
                || error.contains("blocked-unpreserved-worktree-content"),
            "unexpected error: {error}"
        );
        assert!(local.exists(), "clone with new untracked data was deleted");
        assert!(local.join("after-preview.txt").exists());
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
        assert_eq!(record.removed_paths, ["/projects/removed-clone"]);
        assert_eq!(record.completed_unix, 123);
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
