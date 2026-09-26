use clap::{Parser, Subcommand};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs,
    io::{self, Read},
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
    },
    Preserve,
    Preview,
    Cleanup {
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
#[derive(Clone, Serialize, Deserialize, Default)]
struct Saved {
    source: String,
    name: String,
    commit: String,
    remote_ref: String,
    tree: Option<String>,
    verification: String,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Repository {
    path: String,
    common_dir: String,
    kind: String,
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
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c.output()
}
fn out(args: &[String], cwd: Option<&Path>, extra_env: &[(&str, &str)]) -> Result<String, String> {
    let o = run(args, cwd, extra_env).map_err(|e| format!("{}: {e}", args[0]))?;
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
    ];
    a.extend(args.iter().map(|x| x.to_string()));
    out(&a, None, &[("GIT_NO_LAZY_FETCH", "1")])
}
fn git_with_index(path: &Path, args: &[&str], index: &Path) -> Result<String, String> {
    let mut a = vec![
        "git".to_string(),
        "-C".into(),
        path.to_string_lossy().into_owned(),
    ];
    a.extend(args.iter().map(|x| x.to_string()));
    out(
        &a,
        None,
        &[
            ("GIT_NO_LAZY_FETCH", "1"),
            (
                "GIT_INDEX_FILE",
                index.to_str().ok_or("non-UTF8 temporary index")?,
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
fn parse_worktrees(path: &Path) -> Result<Vec<Worktree>, String> {
    let text = git(path, &["worktree", "list", "--porcelain"])?;
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
fn inventory_worktree(
    owner: &Path,
    common: &Path,
    wt: &mut Worktree,
    remote: &str,
) -> Result<(), String> {
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
            "-z",
        ],
    )?;
    wt.stashes = git(&p, &["stash", "list", "--format=%H %gd %gs"])
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let mut nested = Vec::new();
    for e in WalkDir::new(&p)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        match e {
            Ok(x)
                if x.file_type().is_dir()
                    && x.path() != p
                    && (x.path().join(".git").exists() || x.path().join(".git").is_symlink()) =>
            {
                nested.push(x.path().to_string_lossy().into_owned())
            }
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    wt.nested_repositories = nested;
    wt.index_tree = git(&p, &["write-tree"]).ok().map(|x| x.trim().to_owned());
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
    wt.fingerprint = snapshot_fingerprint(wt);
    let _ = owner;
    Ok(())
}
fn inventory_one(path: &Path, matched: Vec<String>, remote: &str) -> Repository {
    let common = common_dir(path).unwrap_or_else(|_| path.to_path_buf());
    let bare = git(path, &["rev-parse", "--is-bare-repository"])
        .map(|x| x.trim() == "true")
        .unwrap_or(false);
    let mut r = Repository {
        path: path.to_string_lossy().into_owned(),
        common_dir: common.to_string_lossy().into_owned(),
        kind: if bare { "bare" } else { "clone" }.into(),
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
        let fsck = git(path, &["fsck", "--full", "--unreachable", "--no-reflogs"])?;
        for line in fsck.lines() {
            let p: Vec<_> = line.split_whitespace().collect();
            if p.len() == 3 && (p[0] == "unreachable" || p[0] == "dangling") {
                if p[1] == "commit" {
                    r.unreachable_commits.push(p[2].into())
                } else {
                    r.unreachable_noncommits.push(p[2].into())
                }
            }
        }
        let mut represented: BTreeSet<String> =
            git(path, &["rev-list", "--objects", "--all", "--reflog"])?
                .lines()
                .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
                .collect();
        for wt in &r.worktrees {
            for t in [&wt.index_tree, &wt.worktree_tree].into_iter().flatten() {
                represented.insert(t.clone());
                for line in git(path, &["ls-tree", "-r", "-t", "--full-tree", t])?.lines() {
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
    let mut seen = HashSet::new();
    for root in roots {
        if !root.exists() {
            gaps.push(format!("missing scan root {}", root.display()));
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
        {
            let e = match entry {
                Ok(v) => v,
                Err(e) => {
                    gaps.push(e.to_string());
                    continue;
                }
            };
            if !e.file_type().is_dir() && !e.file_type().is_file() {
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
                v.1.push(p.to_string_lossy().into_owned());
            }
            if meta.is_dir() {
                continue;
            }
            if !seen.insert(p.to_path_buf()) {
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
    Regex::new(r#"(?i)(-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----|\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16})\b|\b(password|secret|api[_-]?key|access[_-]?token|token|client[_-]?secret)\s*[:=]\s*['\"]?[A-Za-z0-9/+_=-]{8,})"#).unwrap()
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
    let excluded = remote_refs
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect::<BTreeSet<_>>();
    let mut range = oid.to_owned();
    if !excluded.is_empty() {
        range.push_str(" --not ");
        range.push_str(&excluded.iter().copied().collect::<Vec<_>>().join(" "));
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
    let finding_count = serde_json::from_slice::<serde_json::Value>(&findings)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(usize::MAX);
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
        .args(["-C", repo.to_string_lossy().as_ref(), "cat-file", "--batch"])
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
    let mut data = Vec::new();
    child
        .stdout
        .take()
        .ok_or("cat-file stdout unavailable")?
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("object scan incomplete".into());
    }
    let mut pos = 0usize;
    while pos < data.len() {
        let end = data[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("malformed cat-file header")?
            + pos;
        let h = String::from_utf8_lossy(&data[pos..end]).to_string();
        pos = end + 1;
        let fields: Vec<_> = h.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(format!("missing Git object {h}"));
        }
        let size: usize = fields[2].parse().map_err(|_| "bad object length")?;
        if size > 16 * 1024 * 1024 {
            return Err(format!("oversized blob blocks secret scan: {} bytes", size));
        }
        if pos + size > data.len() {
            return Err("truncated object".into());
        }
        if r.is_match(&data[pos..pos + size]) {
            return Err(format!("secret pattern in object {}", fields[0]));
        }
        pos += size + 1;
    }
    Ok(())
}
fn secret_scan(repo: &Path, worktrees: &[Worktree], remote: &str) -> Result<(), String> {
    // Require the scanner. Scan visible files and local-only commit history before any push.
    let dir = out(
        &[
            "gitleaks".into(),
            "dir".into(),
            "--redact".into(),
            "--no-banner".into(),
            repo.to_string_lossy().into_owned(),
        ],
        None,
        &[],
    )
    .map_err(|e| format!("working-file secret scan blocked: {e}"))?;
    let _ = dir;
    for wt in worktrees {
        if wt.missing || wt.foreign_registration {
            continue;
        }
        let p = Path::new(&wt.path);
        let result = out(
            &[
                "gitleaks".into(),
                "dir".into(),
                "--redact".into(),
                "--no-banner".into(),
                p.to_string_lossy().into_owned(),
            ],
            None,
            &[],
        )
        .map_err(|e| format!("worktree secret scan blocked: {e}"))?;
        let _ = result;
    }
    let listing = out(
        &["git".into(), "ls-remote".into(), remote.into()],
        None,
        &[],
    )
    .map_err(|e| format!("cannot list remote refs for secret-scan exclusion: {e}"))?;
    let mut excluded = BTreeSet::new();
    for line in listing.lines() {
        let Some(oid) = line.split_whitespace().next() else {
            continue;
        };
        if git(repo, &["cat-file", "-e", &format!("{oid}^{{commit}}")]).is_ok() {
            excluded.insert(oid.to_owned());
        }
    }
    let mut opts = vec!["--all".to_owned(), "--reflog".to_owned()];
    if !excluded.is_empty() {
        opts.push("--not".into());
        opts.extend(excluded);
    }
    let history_args = vec![
        "gitleaks".into(),
        "git".into(),
        "--redact".into(),
        "--no-banner".into(),
        format!("--log-opts={}", opts.join(" ")),
        repo.to_string_lossy().into_owned(),
    ];
    out(&history_args, None, &[]).map_err(|e| format!("local-history secret scan blocked: {e}"))?;
    // The object scanner checks every blob reachable from each preserved commit too.
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
fn push_ref(remote: &str, repo: &Path, oid: &str, reference: &str) -> Result<(), String> {
    match remote_oid(remote, reference)? {
        Some(existing) if existing == oid => return Ok(()),
        Some(_) => {
            return Err(format!(
                "remote ref collision; refusing overwrite: {reference}"
            ));
        }
        None => {}
    }
    let a = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "push".into(),
        remote.into(),
        format!("{oid}:{reference}"),
    ];
    out(&a, None, &[]).map(|_| ())
}
fn preserve(m: &mut Manifest) -> Result<(), String> {
    for r in &mut m.repositories {
        r.preservation = "blocked".into();
        if !r.inventory_complete {
            r.verification_error = Some("incomplete Git inventory".into());
            continue;
        }
        if !r.alternates.is_empty() {
            r.verification_error = Some("object alternates are not independently preserved".into());
            continue;
        }
        // Tags, custom refs and reflog-only data carry metadata that this phase does
        // not yet serialize. Refuse the whole copy instead of silently omitting it.
        let unsupported = r.refs.iter().find(|x| {
            !x.starts_with("refs/heads/")
                && !x.starts_with("refs/remotes/")
                && !x.starts_with("refs/stash ")
        });
        if let Some(x) = unsupported {
            r.verification_error = Some(format!("unsupported local ref blocks preservation: {x}"));
            continue;
        }
        let p = PathBuf::from(&r.path);
        if r.kind == "bare" {
            r.verification_error = Some("bare stores require tag/ref metadata preservation".into());
            continue;
        }
        if r.worktrees.iter().any(|w| {
            w.missing
                || w.foreign_registration
                || !w.ignored.is_empty()
                || !w.nested_repositories.is_empty()
        }) {
            r.verification_error = Some("missing/foreign worktree, ignored content, or nested repository blocks preservation".into());
            continue;
        }
        let gitlinks = git(&p, &["ls-files", "--stage"])
            .map(|s| s.lines().any(|l| l.starts_with("160000 ")))
            .unwrap_or(true);
        if gitlinks || p.join(".gitmodules").exists() {
            r.verification_error = Some("submodule dependency blocks preservation".into());
            continue;
        }
        if let Err(e) = secret_scan(&p, &r.worktrees, &m.remote) {
            r.verification_error = Some(e);
            continue;
        }
        let mut error = None;
        let mut mapped = BTreeSet::new();
        let branches = r.branches.clone();
        let refs = r.refs.clone();
        let unreachable = r.unreachable_commits.clone();
        let stashes = r.stashes.clone();
        let worktrees = r.worktrees.clone();
        let remote = m.remote.clone();
        let common = r.common_dir.clone();
        for b in &branches {
            if let Err(e) = save_object(
                &remote,
                &common,
                r,
                &p,
                &format!("branch:{}", b.name),
                &b.commit,
                "branch",
            ) {
                error = Some(e);
                break;
            }
            mapped.insert(b.commit.clone());
        }
        if error.is_none() {
            for reference in &refs {
                let Some((name, tail)) = reference.split_once(' ') else {
                    error = Some("malformed ref inventory".into());
                    break;
                };
                let oid = tail.split_whitespace().next().unwrap_or("");
                if !name.starts_with("refs/remotes/") || mapped.contains(oid) {
                    continue;
                }
                if let Err(e) = save_object(
                    &remote,
                    &common,
                    r,
                    &p,
                    &format!("tracking:{name}"),
                    oid,
                    "tracking",
                ) {
                    error = Some(e);
                    break;
                }
                mapped.insert(oid.to_owned());
            }
        }
        if error.is_none() {
            for oid in &unreachable {
                if let Err(e) = save_object(
                    &remote,
                    &common,
                    r,
                    &p,
                    &format!("unreachable:{oid}"),
                    oid,
                    "unreachable",
                ) {
                    error = Some(e);
                    break;
                }
                mapped.insert(oid.clone());
            }
        }
        if error.is_none() {
            for (i, stash) in stashes.iter().enumerate() {
                let oid = stash.split_whitespace().next().unwrap_or("");
                if oid.len() < 40 {
                    error = Some("malformed stash inventory".into());
                    break;
                }
                if let Err(e) =
                    save_object(&remote, &common, r, &p, &format!("stash:{i}"), oid, "stash")
                {
                    error = Some(e);
                    break;
                }
            }
        }
        if error.is_none() {
            for w in &worktrees {
                if let Some(head) = &w.head {
                    if !mapped.contains(head) {
                        if let Err(e) = save_object(
                            &remote,
                            &common,
                            r,
                            &p,
                            &format!("detached:{}", w.path),
                            head,
                            "detached",
                        ) {
                            error = Some(e);
                            break;
                        }
                        mapped.insert(head.clone());
                    }
                }
                if error.is_some() {
                    break;
                }
                if let Err(e) = save_worktree(&remote, &common, r, w) {
                    error = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = error {
            r.verification_error = Some(e);
            continue;
        }
        if !r.unreachable_noncommits.is_empty() {
            let mut represented = BTreeSet::new();
            for item in &r.saved {
                if let Ok(objects) = git(&p, &["rev-list", "--objects", &item.commit]) {
                    represented.extend(
                        objects
                            .lines()
                            .filter_map(|x| x.split_whitespace().next().map(str::to_owned)),
                    );
                }
            }
            if let Some(oid) = r
                .unreachable_noncommits
                .iter()
                .find(|oid| !represented.contains(*oid))
            {
                r.verification_error = Some(format!(
                    "unreachable object is not represented by saved commits: {oid}"
                ));
                continue;
            }
        }
        r.preservation = "complete".into();
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
    push_ref(remote, repo, oid, &full)?;
    r.saved.push(Saved {
        source: kind.into(),
        name: name.into(),
        commit: oid.into(),
        remote_ref: rr,
        tree: None,
        verification: "push-succeeded".into(),
    });
    Ok(())
}

fn write_tree(repo: &Path, head: &str, add_worktree: bool) -> Result<String, String> {
    let index = env::temp_dir().join(format!(
        "far-index-{}-{}",
        std::process::id(),
        hash_name(&format!("{}-{head}-{add_worktree}", repo.display()))
    ));
    let _ = fs::remove_file(&index);
    let envs = [(
        "GIT_INDEX_FILE",
        index.to_str().ok_or("invalid temp index path")?,
    )];
    let mut args = vec![
        "git".to_string(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
        "read-tree".into(),
    ];
    args.push(if head.is_empty() {
        "--empty".into()
    } else {
        head.into()
    });
    out(&args, None, &envs)?;
    if add_worktree {
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
    }
    let result = out(
        &[
            "git".into(),
            "-C".into(),
            repo.to_string_lossy().into_owned(),
            "write-tree".into(),
        ],
        None,
        &envs,
    )
    .map(|x| x.trim().into());
    let _ = fs::remove_file(&index);
    result
}

fn create_snapshot_commit(
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
    out(&args, None, &[]).map(|x| x.trim().into())
}

fn save_worktree(
    remote: &str,
    common: &str,
    r: &mut Repository,
    w: &Worktree,
) -> Result<(), String> {
    let p = Path::new(&w.path);
    let head = w.head.as_deref().unwrap_or("");
    let staged = git(p, &["write-tree"])?;
    let head_tree = if head.is_empty() {
        String::new()
    } else {
        git(p, &["rev-parse", &format!("{head}^{{tree}}")])?
            .trim()
            .into()
    };
    let full_tree = write_tree(p, head, true)?;
    if staged.trim() == head_tree && full_tree == head_tree {
        return Ok(());
    }
    if staged.trim() != head_tree && staged.trim() != full_tree {
        let c = create_snapshot_commit(
            p,
            staged.trim(),
            (!head.is_empty()).then_some(head),
            "recovery staged snapshot",
        )?;
        scan_commit(p, &c, remote)?;
        let rr = recovery_ref(
            &Repository {
                common_dir: common.into(),
                ..Default::default()
            },
            "staged",
            &w.path,
            &c,
        );
        push_ref(remote, p, &c, &format!("refs/heads/{rr}"))?;
        r.saved.push(Saved {
            source: "staged-snapshot".into(),
            name: w.path.clone(),
            commit: c.clone(),
            remote_ref: rr,
            tree: Some(staged.trim().into()),
            verification: "push-succeeded".into(),
        });
        // Keep owned parent string alive through complete snapshot creation.
        let work =
            create_snapshot_commit(p, &full_tree, Some(&c), "recovery full worktree snapshot")?;
        scan_commit(p, &work, remote)?;
        let rr = recovery_ref(
            &Repository {
                common_dir: common.into(),
                ..Default::default()
            },
            "worktree",
            &w.path,
            &work,
        );
        push_ref(remote, p, &work, &format!("refs/heads/{rr}"))?;
        r.saved.push(Saved {
            source: "worktree-snapshot".into(),
            name: w.path.clone(),
            commit: work,
            remote_ref: rr,
            tree: Some(full_tree),
            verification: "push-succeeded".into(),
        });
    } else {
        let work = create_snapshot_commit(
            p,
            &full_tree,
            (!head.is_empty()).then_some(head),
            "recovery full worktree snapshot",
        )?;
        scan_commit(p, &work, remote)?;
        let rr = recovery_ref(
            &Repository {
                common_dir: common.into(),
                ..Default::default()
            },
            "worktree",
            &w.path,
            &work,
        );
        push_ref(remote, p, &work, &format!("refs/heads/{rr}"))?;
        r.saved.push(Saved {
            source: "worktree-snapshot".into(),
            name: w.path.clone(),
            commit: work,
            remote_ref: rr,
            tree: Some(full_tree),
            verification: "push-succeeded".into(),
        });
    }
    Ok(())
}

fn same_inventory(expected: &Repository, current: &Repository) -> bool {
    let original_unreachable: BTreeSet<_> = expected.unreachable_commits.iter().collect();
    let saved_commits: BTreeSet<_> = expected.saved.iter().map(|s| &s.commit).collect();
    let current_unreachable: BTreeSet<_> = current.unreachable_commits.iter().collect();
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
        && added_unreachable
            .iter()
            .all(|oid| saved_commits.contains(*oid))
        && original_unreachable.is_subset(&current_unreachable)
        && expected.unreachable_noncommits == current.unreachable_noncommits
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
                    && a.missing == b.missing
                    && a.foreign_registration == b.foreign_registration
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
    if r.saved.is_empty() || r.saved.iter().any(|s| s.verification != "push-succeeded") {
        return Some("blocked-no-successful-push-record".into());
    }
    if !r.alternates.is_empty() {
        return Some("blocked-shared-storage".into());
    }
    if r.kind != "clone" {
        return Some("blocked-repository-kind".into());
    }
    if r.worktrees.iter().any(|w| {
        w.missing
            || w.foreign_registration
            || !w.ignored.is_empty()
            || !w.nested_repositories.is_empty()
    }) {
        return Some("blocked-worktree-dependency-or-unpreserved-files".into());
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
fn cleanup_repository(r: &Repository, remote: &str, state: &Path) -> Result<Vec<String>, String> {
    if let Some(reason) = cleanup_blocker(r) {
        return Err(reason);
    }
    let owner = PathBuf::from(&r.path);
    let md = fs::symlink_metadata(&owner).map_err(|e| e.to_string())?;
    if !md.is_dir() || md.file_type().is_symlink() {
        return Err("repository path is not an exact real directory".into());
    }
    let live = inventory_one(&owner, r.matched_paths.clone(), remote);
    if !same_inventory(r, &live) {
        return Err("local refs/worktrees changed since push".into());
    }
    let owner_canon = fs::canonicalize(&owner).map_err(|e| e.to_string())?;
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
    let current = inventory_one(&owner, r.matched_paths.clone(), remote);
    let root_before = r
        .worktrees
        .iter()
        .find(|w| fs::canonicalize(&w.path).ok().as_deref() == Some(owner_canon.as_path()))
        .ok_or("owner root worktree missing from inventory")?;
    if !current.inventory_complete
        || current.path != r.path
        || current.common_dir != r.common_dir
        || current.branches != r.branches
        || current.refs != r.refs
        || current.stashes != r.stashes
        || current.worktrees.len() != 1
        || current.worktrees[0].path != root_before.path
        || current.worktrees[0].fingerprint != root_before.fingerprint
    {
        return Err("repository changed while removing worktrees".into());
    }
    fs::remove_dir_all(&owner)
        .map_err(|e| format!("remove exact clone {}: {e}", owner.display()))?;
    removed.push(owner.to_string_lossy().into_owned());
    Ok(removed)
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
        Phase::Scan { roots } => {
            let roots = if roots.is_empty() {
                vec![
                    "/Users".into(),
                    "/Volumes".into(),
                    "/private".into(),
                    "/opt".into(),
                    "/tmp".into(),
                    "/private/tmp".into(),
                    "/Applications".into(),
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
    fn preserves_staged_and_complete_worktree_without_touching_main() {
        let (_t, local, remote, remote_s) = fixture();
        fs::write(local.join("tracked.txt"), "staged\n").unwrap();
        cmd(&["git", "-C", local.to_str().unwrap(), "add", "tracked.txt"]);
        fs::write(local.join("tracked.txt"), "working\n").unwrap();
        fs::write(local.join("new.txt"), "untracked payload\n").unwrap();
        let original_main = cmd(&[
            "git",
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        let r = &m.repositories[0];
        assert_eq!(r.preservation, "complete", "{:?}", r.verification_error);
        let staged = r
            .saved
            .iter()
            .find(|s| s.source == "staged-snapshot")
            .unwrap();
        let full = r
            .saved
            .iter()
            .find(|s| s.source == "worktree-snapshot")
            .unwrap();
        assert_eq!(
            fetched_tree(&remote, &staged.commit, "tracked.txt"),
            "staged"
        );
        assert_eq!(
            fetched_tree(&remote, &full.commit, "tracked.txt"),
            "working"
        );
        assert_eq!(
            fetched_tree(&remote, &full.commit, "new.txt"),
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
        assert!(r.saved.iter().all(|s| !s.remote_ref.ends_with("/main")));
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
    fn stash_and_unreachable_commits_are_mapped_but_tag_blocks() {
        let (_t, local, remote, remote_s) = fixture();
        fs::write(local.join("tracked.txt"), "stash content\n").unwrap();
        cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "stash",
            "push",
            "-m",
            "fixture",
        ]);
        let tree = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "rev-parse",
            "HEAD^{tree}",
        ]);
        let dangling = cmd(&[
            "git",
            "-C",
            local.to_str().unwrap(),
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit-tree",
            &tree,
            "-m",
            "dangling",
        ]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(
            m.repositories[0].preservation, "complete",
            "{:?}",
            m.repositories[0].verification_error
        );
        assert!(m.repositories[0].saved.iter().any(|s| s.source == "stash"));
        assert!(
            m.repositories[0]
                .saved
                .iter()
                .any(|s| s.source == "unreachable" && s.commit == dangling)
        );

        let (_t, local, remote, remote_s) = fixture();
        cmd(&["git", "-C", local.to_str().unwrap(), "tag", "local-only"]);
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        assert_eq!(m.repositories[0].preservation, "blocked");
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
    fn deletes_only_after_all_branch_and_worktree_snapshots_push() {
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
    fn cleanup_refuses_changed_worktree_after_push() {
        let (_t, local, _remote, remote_s) = fixture();
        let mut m = manifest(&local, &remote_s);
        preserve(&mut m).unwrap();
        preview(&mut m);
        fs::write(local.join("tracked.txt"), "late edit\n").unwrap();
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
    fn secret_scan_blocks_canary_commit() {
        let d = repo();
        let oid = commit(
            d.path(),
            b"github_token=ghp_abcdefghijklmnopqrstuvwxyz1234567890\n",
        );
        assert!(scan_commit(d.path(), &oid, "").is_err());
    }
}
