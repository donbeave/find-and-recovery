mod dedupe;

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
    Preserve,
    Preview,
    Cleanup {
        #[arg(long)]
        execute: bool,
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

#[derive(Clone, Serialize, Deserialize, Default)]
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
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
struct Branch {
    name: String,
    commit: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
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
    inventory_complete: bool,
    inventory_errors: Vec<String>,
    saved: Vec<Saved>,
    preservation: String,
    verification: String,
    verification_error: Option<String>,
    deletion: String,
}
#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    schema_version: u32,
    remote: String,
    generated_unix: u64,
    roots: Vec<String>,
    coverage_gaps: Vec<String>,
    repositories: Vec<Repository>,
    deleted: Vec<String>,
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
fn common_dir(path: &Path) -> Result<PathBuf, String> {
    let s = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(s.trim()))
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
            if git_with_index(&p, &["add", "-A", "-f", "--", "."], &index).is_ok() {
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
    let mut reachable = BTreeSet::new();
    for reference in refs.lines() {
        let Ok(tip) = git(repo, &["rev-parse", &format!("{reference}^{{commit}}")]) else {
            continue;
        };
        let rows = git(repo, &["rev-list", "--objects", tip.trim()])?;
        reachable.extend(
            rows.lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_owned)),
        );
    }
    for worktree in worktrees {
        if let Some(head) = worktree.head.as_deref() {
            let rows = git(repo, &["rev-list", "--objects", head])?;
            reachable.extend(
                rows.lines()
                    .filter_map(|line| line.split_whitespace().next().map(str::to_owned)),
            );
        }
        for tree in [&worktree.index_tree].into_iter().flatten() {
            let root_tree = git(repo, &["rev-parse", &format!("{tree}^{{tree}}")])?
                .trim()
                .to_owned();
            reachable.insert(root_tree.clone());
            let rows = git(repo, &["ls-tree", "-r", "-t", "--full-tree", &root_tree])?;
            reachable.extend(
                rows.lines()
                    .filter_map(|line| line.split_whitespace().nth(2).map(str::to_owned)),
            );
        }
    }
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
        // This workflow inventories refs and detached worktree HEADs. A full
        // `git fsck --unreachable` is intentionally omitted: it can consume
        // gigabytes in large object stores, and unreachable objects are not
        // part of the user's branch-push scope.
        let mut represented = reachable_objects(path, &r.worktrees)?;
        for wt in &r.worktrees {
            for t in [&wt.index_tree].into_iter().flatten() {
                let root_tree = git(path, &["rev-parse", &format!("{t}^{{tree}}")])?
                    .trim()
                    .to_owned();
                represented.insert(root_tree.clone());
                for line in git(path, &["ls-tree", "-r", "-t", "--full-tree", &root_tree])?.lines()
                {
                    if let Some(oid) = line.split_whitespace().nth(2) {
                        represented.insert(oid.into());
                    }
                }
            }
        }
        r.unreachable_noncommits
            .retain(|oid| !represented.contains(oid));
        match git(path, &["lfs", "ls-files", "--all", "--long"]) {
            Ok(x) => r.lfs_files = x.lines().map(str::to_owned).collect(),
            Err(e) => return Err(format!("could not inventory LFS payloads: {e}")),
        }
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
fn discover(roots: &[PathBuf], remote: &str) -> (Vec<Repository>, Vec<String>) {
    let mut stores: BTreeMap<String, (PathBuf, Vec<String>)> = BTreeMap::new();
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
        let mut matched_roots = HashSet::<PathBuf>::new();
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.file_name() == ".git" {
                    return false;
                }
                let p = e.path();
                if matched_roots
                    .iter()
                    .any(|root| p != root && p.starts_with(root))
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
                let owner = if common.file_name().is_some_and(|n| n == ".git") {
                    common.parent().unwrap_or(p).to_path_buf()
                } else {
                    p.to_path_buf()
                };
                let key = common.to_string_lossy().into_owned();
                let v = stores.entry(key).or_insert_with(|| (owner, Vec::new()));
                let path = p.to_string_lossy().into_owned();
                if !v.1.contains(&path) {
                    v.1.push(path);
                }
            }
            if meta.is_dir() {
                continue;
            }
        }
    }
    let mut repos = Vec::new();
    for (_, (owner, mut matches)) in stores {
        matches.sort();
        if let Ok(wts) = parse_worktrees(&owner) {
            for wt in wts {
                let p = PathBuf::from(&wt.path);
                if p.exists() && target_path(&p, remote) && !matches.contains(&wt.path) {
                    matches.push(wt.path);
                }
            }
        }
        repos.push(inventory_one(&owner, matches, remote));
    }
    (repos, gaps)
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
fn scan_commit(repo: &Path, oid: &str, remote: &str) -> Result<(), String> {
    // Run the maintained scanner against the exact commit ancestry before any
    // object upload. Never persist scanner output or print a finding.
    let remote_refs = if remote.is_empty() {
        String::new()
    } else {
        out(
            &["git".into(), "ls-remote".into(), remote.into()],
            None,
            &[],
        )
        .map_err(|e| format!("cannot inspect remote history for secret scanning: {e}"))?
    };
    // `ls-remote` can advertise commit IDs absent from this clone (shallow
    // histories, stale remote refs). `rev-list --not` rejects those IDs, so
    // exclude only remote commits that are present in this object database.
    let mut excluded = BTreeSet::new();
    for oid in remote_refs
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        if git(repo, &["cat-file", "-e", &format!("{oid}^{{commit}}")]).is_ok() {
            excluded.insert(oid.to_owned());
        }
    }
    let mut range = oid.to_owned();
    if !excluded.is_empty() {
        range.push_str(" --not ");
        range.push_str(&excluded.iter().cloned().collect::<Vec<_>>().join(" "));
    }
    let report = env::temp_dir().join(format!(
        "far-gitleaks-{}-{}.json",
        std::process::id(),
        hash_name(oid)
    ));
    let scan = vec![
        "gitleaks".into(),
        "git".into(),
        "--no-banner".into(),
        "--redact".into(),
        "--log-opts".into(),
        range.clone(),
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
    let mut rev_args = vec![
        "rev-list".to_owned(),
        "--objects".to_owned(),
        oid.to_owned(),
    ];
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
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .map_err(|e| e.to_string())?;
    }
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
    if !child.wait().map_err(|e| e.to_string())?.success() {
        return Err("object scan incomplete".into());
    }
    Ok(())
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
    let output = out(&a, None, &[])?;
    Ok(output.lines().any(|line| {
        line.starts_with("*")
            && line
                .split_whitespace()
                .any(|field| field == reference || field.ends_with(&format!(":{reference}")))
    }))
}
fn preserve(m: &mut Manifest) -> Result<(), String> {
    for r in &mut m.repositories {
        let prior_saved = r.saved.clone();
        r.preservation = "blocked".into();
        r.saved.clear();
        r.verification_error = None;
        if !r.inventory_complete {
            r.verification_error = Some("incomplete Git inventory".into());
            continue;
        }
        if !r.alternates.is_empty() {
            r.verification_error = Some("shared object alternates block cleanup".into());
            continue;
        }
        if !r.lfs_files.is_empty() {
            r.verification_error =
                Some("LFS payloads are not scanned or transferred by this workflow".into());
            continue;
        }
        if r.worktrees.iter().any(|w| w.foreign_registration) {
            r.verification_error = Some("missing or foreign registered worktree".into());
            continue;
        }
        let path = PathBuf::from(&r.path);
        let remote = m.remote.clone();
        let live = inventory_one(&path, r.matched_paths.clone(), &remote);
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
        if !live.inventory_complete
            || live.branches != r.branches
            || heads(&live.worktrees) != heads(&r.worktrees)
        {
            r.verification_error =
                Some("local branches or worktree HEADs changed since scan".into());
            continue;
        }
        let common = r.common_dir.clone();
        let branches = r.branches.clone();
        let worktrees = r.worktrees.clone();
        let stashes = r.stashes.clone();
        let unreachable = r.unreachable_commits.clone();
        let mut mapped = BTreeSet::new();
        let mut failure = None;
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
                failure = Some(error);
                break;
            }
            mapped.insert(branch.commit);
        }
        if failure.is_none() {
            for (index, stash) in stashes.iter().enumerate() {
                let Some(oid) = stash.split_whitespace().next() else {
                    failure = Some("malformed stash inventory".into());
                    break;
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
                    failure = Some(error);
                    break;
                }
            }
        }
        if failure.is_none() {
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
                    failure = Some(error);
                    break;
                }
                mapped.insert(oid);
            }
        }
        if failure.is_none() {
            for worktree in worktrees {
                if worktree.bare {
                    continue;
                }
                if !worktree.ignored.is_empty() {
                    failure = Some(format!(
                        "worktree has ignored content; snapshot blocked: {}",
                        worktree.path
                    ));
                    break;
                }
                if let Err(error) = validate_nested_inventory(&worktree) {
                    failure = Some(error);
                    break;
                }
                if worktree.detached {
                    let Some(head) = worktree.head.as_deref() else {
                        failure = Some(format!("detached worktree has no HEAD: {}", worktree.path));
                        break;
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
                            failure = Some(error);
                            break;
                        }
                        mapped.insert(head.to_owned());
                    }
                }
                if !worktree.missing {
                    if let Err(error) = save_worktree(&remote, &common, r, &worktree) {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
        if failure.is_none() && !r.unreachable_noncommits.is_empty() {
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
                failure = Some(format!(
                    "unreachable object not included in pushed recovery commits: {oid}"
                ));
            }
        }
        if let Some(error) = failure {
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
    }
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
    let full = format!("refs/heads/{rr}");
    let owned_remote_ref = push_ref(remote, repo, oid, &full)?;
    r.saved.push(Saved {
        source: kind.into(),
        name: name.into(),
        commit: oid.into(),
        remote_ref: rr,
        created_by_this_run: owned_remote_ref,
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
        "-f".into(),
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
        scan_commit(path, &commit, remote)?;
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
        let created = push_ref(remote, path, &commit, &format!("refs/heads/{reference}"))?;
        repo.saved.push(Saved {
            source: "staged-snapshot".into(),
            name,
            commit: commit.clone(),
            remote_ref: reference,
            created_by_this_run: created,
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
    scan_commit(path, &commit, remote)?;
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
    let created = push_ref(remote, path, &commit, &format!("refs/heads/{reference}"))?;
    repo.saved.push(Saved {
        source: "worktree-snapshot".into(),
        name,
        commit,
        remote_ref: reference,
        created_by_this_run: created,
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
fn cleanup_blocker(r: &Repository) -> Option<String> {
    if !r.inventory_complete {
        return Some("blocked-incomplete-inventory".into());
    }
    if r.preservation != "complete" {
        return Some("blocked-preservation".into());
    }
    if r.saved
        .iter()
        .any(|saved| saved.verification != "push-succeeded")
    {
        return Some("blocked-push-not-successful".into());
    }
    if !r.lfs_files.is_empty() {
        return Some("blocked-LFS-payloads".into());
    }
    for worktree in r.worktrees.iter().filter(|w| !w.ignored.is_empty()) {
        if !r.saved.iter().any(|saved| {
            saved.source == "worktree-snapshot"
                && saved.name == format!("worktree:{}", worktree.path)
                && saved.verification == "push-succeeded"
        }) {
            return Some(format!(
                "blocked-unpreserved-ignored-content:{}",
                worktree.path
            ));
        }
    }
    if !r.alternates.is_empty() {
        return Some("blocked-shared-storage".into());
    }
    if r.kind != "clone" && r.kind != "bare" {
        return Some("blocked-repository-kind".into());
    }
    if r.worktrees.iter().any(|w| {
        w.foreign_registration
            || w.nested_repositories
                .iter()
                .any(|item| skipped_tree_path(item).is_none())
    }) {
        return Some("blocked-worktree-dependency".into());
    }
    for b in &r.branches {
        if !r.saved.iter().any(|s| {
            s.source == "branch"
                && s.name == format!("branch:{}", b.name)
                && s.commit == b.commit
                && s.verification == "push-succeeded"
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
                    && s.verification == "push-succeeded"
            }) {
                return Some(format!("blocked-unpushed-detached-head:{}", w.path));
            }
        }
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
fn preview(m: &mut Manifest) {
    for r in &mut m.repositories {
        r.deletion = cleanup_blocker(r).unwrap_or_else(|| "eligible".into());
    }
}
fn cleanup_recheck(r: &Repository, remote: &str) -> Result<(), String> {
    let owner = Path::new(&r.path);
    if !target_path(owner, remote) {
        return Err("repository remote no longer matches requested target".into());
    }
    let branch_text = git(
        owner,
        &[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads",
        ],
    )?;
    let branches: Vec<Branch> = branch_text
        .lines()
        .filter_map(|line| {
            let (name, commit) = line.split_once(' ')?;
            Some(Branch {
                name: name.into(),
                commit: commit.into(),
            })
        })
        .collect();
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
    let stashes = git(owner, &["stash", "list", "--format=%H %gd %gs"])?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
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
        let reference = format!("refs/heads/{}", saved.remote_ref);
        if remote_oid(remote, &reference)?.as_deref() != Some(saved.commit.as_str()) {
            return Err(format!(
                "pushed ref missing or changed: {}",
                saved.remote_ref
            ));
        }
    }
    Ok(())
}
fn cleanup_repository(r: &Repository, remote: &str, state: &Path) -> Result<Vec<String>, String> {
    if let Some(reason) = cleanup_blocker(r) {
        return Err(reason);
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
        let reference = format!("refs/heads/{}", saved.remote_ref);
        if remote_oid(remote, &reference)?.as_deref() != Some(saved.commit.as_str()) {
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
fn dedupe(m: &mut Manifest, state: &Path, execute: bool) -> Result<(), String> {
    if m.repositories.iter().any(|repo| repo.deletion != "deleted") {
        return Err("dedupe requires local cleanup to finish for all manifest repositories".into());
    }
    let remote = m.remote.clone();
    reject_remote_url_rewrite(&remote, None)?;
    let heads = out(
        &[
            "git".into(),
            "ls-remote".into(),
            "--heads".into(),
            remote.clone(),
        ],
        None,
        &[],
    )?;
    let branches = heads
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some(dedupe::RemoteBranch {
                name: name.strip_prefix("refs/heads/")?.into(),
                oid: oid.into(),
            })
        })
        .collect::<Vec<_>>();
    let sym = out(
        &[
            "git".into(),
            "ls-remote".into(),
            "--symref".into(),
            remote.clone(),
            "HEAD".into(),
        ],
        None,
        &[],
    )?;
    let default = sym.lines().find_map(|line| {
        line.strip_prefix("ref: refs/heads/")?
            .split_whitespace()
            .next()
            .map(str::to_owned)
    });
    let ownership = m
        .repositories
        .iter()
        .filter(|repo| repo.deletion == "deleted")
        .flat_map(|repo| repo.saved.iter())
        .filter(|saved| recovery_ref_is_owned_name(&saved.remote_ref))
        .map(|saved| dedupe::RecoveryRef {
            name: saved.remote_ref.clone(),
            created_by_this_run: saved.created_by_this_run,
        });
    let mut protected: BTreeSet<String> = ["main".into(), "master".into()].into_iter().collect();
    if let Some(name) = default {
        protected.insert(name);
    }
    let groups = dedupe::exact_duplicate_groups(branches, ownership, &protected);
    for group in groups {
        let keep = group
            .branches
            .iter()
            .find(|name| !group.delete_candidates.contains(name))
            .cloned()
            .unwrap_or_default();
        println!(
            "{}\\t{}\\tkeep:{}\\tdelete:{}",
            group.oid,
            group.branches.join(","),
            keep,
            group.delete_candidates.join(",")
        );
        for name in group.delete_candidates {
            if !recovery_ref_is_owned_name(&name) {
                continue;
            }
            if !execute {
                println!("preview-delete\\t{name}\\tkeep\\t{keep}");
                continue;
            }
            if remote_oid(&remote, &format!("refs/heads/{name}"))?.as_deref()
                != Some(group.oid.as_str())
            {
                eprintln!("retained {name}; remote tip changed");
                continue;
            }
            if remote_oid(&remote, &format!("refs/heads/{keep}"))?.as_deref()
                != Some(group.oid.as_str())
            {
                eprintln!("retained {name}; duplicate target changed: {keep}");
                continue;
            }
            out(
                &[
                    "git".into(),
                    "push".into(),
                    remote.clone(),
                    "--delete".into(),
                    name.clone(),
                ],
                None,
                &[],
            )?;
            for repo in &mut m.repositories {
                if repo.deletion == "deleted" {
                    for saved in &mut repo.saved {
                        if saved.remote_ref == name && saved.created_by_this_run {
                            saved.retained_ref = Some(keep.clone());
                        }
                    }
                }
            }
            save(m, state)?;
            println!("deleted\\t{name}");
        }
    }
    Ok(())
}

fn recovery_ref_is_owned_name(name: &str) -> bool {
    name.starts_with("recovery/") && !name.split('/').any(|part| part == "..")
}

fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let state = cli.state.canonicalize().unwrap_or(cli.state.clone());
    let mut m = match &cli.command {
        Phase::Scan { .. } => Manifest {
            schema_version: 1,
            remote: cli.remote.clone(),
            generated_unix: now(),
            ..Default::default()
        },
        _ => load(&state)?,
    };
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
                    "/".into(),
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
            let (r, g) = discover(&roots, &m.remote);
            m.repositories = r;
            m.coverage_gaps = g;
            m.generated_unix = now();
            save(&m, &state)?;
            println!("{}", serde_json::to_string_pretty(&m).unwrap());
        }
        Phase::Preserve => {
            preserve(&mut m)?;
            save(&m, &state)?;
            println!("preservation recorded; inspect manifest")
        }
        Phase::Preview => {
            preview(&mut m);
            save(&m, &state)?;
            for r in &m.repositories {
                println!("{}\t{}", r.deletion, r.path)
            }
        }
        Phase::Cleanup { execute } => {
            preview(&mut m);
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
                    match cleanup_repository(&m.repositories[i], &m.remote, &state) {
                        Ok(paths) => {
                            m.repositories[i].deletion = "deleted".into();
                            m.deleted.extend(paths);
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
                m.deleted.push(path.to_string_lossy().into_owned());
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
        preview(&mut m);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        cleanup_repository(&m.repositories[0], &remote_s, &state).unwrap();
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
        preview(&mut m);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        let removed = cleanup_repository(&m.repositories[0], &remote_s, &state).unwrap();
        assert_eq!(removed, vec![local.to_string_lossy().into_owned()]);
        assert!(!local.exists());
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
                .contains("LFS payloads are not scanned or transferred")
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
        preview(&mut m);
        assert_eq!(m.repositories[0].deletion, "eligible");
        let state = local.parent().unwrap().join("external-state");
        fs::create_dir_all(&state).unwrap();
        let linked_canon = fs::canonicalize(&linked).unwrap();
        let removed = cleanup_repository(&m.repositories[0], &remote_s, &state).unwrap();
        assert!(removed.iter().any(|p| Path::new(p) == linked_canon));
        assert!(!linked.exists(), "linked worktree still exists");
        assert!(removed.iter().any(|p| p == local.to_str().unwrap()));
        assert!(!local.exists());
        assert!(!linked.exists());
    }

    #[test]
    fn cleanup_refuses_changed_branch_after_push() {
        let (_t, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        preview(&mut m);
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
        assert!(cleanup_repository(&m.repositories[0], &remote_s, &state).is_err());
        assert!(local.exists());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn deletes_only_created_duplicate_recovery_refs_and_keeps_main() {
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
        let mut manifest = Manifest {
            remote: remote_s.clone(),
            repositories: vec![Repository {
                deletion: "deleted".into(),
                saved: refs
                    .iter()
                    .map(|name| Saved {
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

        dedupe(&mut manifest, &state, true).unwrap();

        assert_eq!(
            remote_oid(&remote_s, "refs/heads/main").unwrap().as_deref(),
            Some(oid.as_str())
        );
        for name in refs {
            assert_eq!(
                remote_oid(&remote_s, &format!("refs/heads/{name}")).unwrap(),
                None
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
