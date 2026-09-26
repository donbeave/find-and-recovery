//! Remote branch deletion with exact leases and transaction-scoped anchors.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const RETENTION: &str = "refs/tags/find-and-recovery-retention";
const REF_BATCH: usize = 64;
static TRANSACTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub remote_ref: String,
    pub expected_oid: String,
    pub keeper_ref: String,
    pub keeper_oid: String,
}

/// Delete reviewed candidates only if their OIDs still match.
///
/// The caller must validate destination identity, server branch protection,
/// and open PR head/base facts. This API cannot infer those from Git alone.
/// Each unique required keeper OID gets a fresh tag in the same atomic server
/// transaction as candidate deletion. No unconditional fallback exists.
pub fn delete_candidates_if_unchanged(
    remote: &str,
    candidates: &[Candidate],
) -> Result<(), String> {
    if candidates.is_empty() {
        return Ok(());
    }
    reject_url_rewrites()?;
    let remote = resolve_push_remote(remote)?;
    let default = default_branch(&remote)?;
    validate_candidates(candidates, &default)?;
    let mut expected = BTreeMap::new();
    for item in candidates {
        insert_expected(&mut expected, &item.remote_ref, &item.expected_oid)?;
        insert_expected(&mut expected, &item.keeper_ref, &item.keeper_oid)?;
    }
    require_tips(&remote, &expected)?;
    let isolated = Isolated::new()?;
    verify_histories(&isolated, &remote, candidates)?;
    if default_branch(&remote)? != default {
        return Err("remote default branch changed during planning; refusing".into());
    }
    require_tips(&remote, &expected)?;
    atomic_delete(&isolated, &remote, candidates)
}

#[deprecated(note = "use delete_candidates_if_unchanged")]
pub fn delete_if_unchanged(
    remote: &str,
    candidate_ref: &str,
    candidate_oid: &str,
    keeper_ref: &str,
    keeper_oid: &str,
) -> Result<(), String> {
    delete_candidates_if_unchanged(
        remote,
        &[Candidate {
            remote_ref: candidate_ref.to_owned(),
            expected_oid: candidate_oid.to_owned(),
            keeper_ref: keeper_ref.to_owned(),
            keeper_oid: keeper_oid.to_owned(),
        }],
    )
}

fn validate_candidates(items: &[Candidate], default: &str) -> Result<(), String> {
    let mut seen = HashSet::new();
    for item in items {
        validate_branch(&item.remote_ref)?;
        validate_branch(&item.keeper_ref)?;
        if item.remote_ref == "refs/heads/main"
            || item.remote_ref == "refs/heads/master"
            || item.remote_ref == default
        {
            return Err(format!(
                "refusing to delete protected branch {}",
                item.remote_ref
            ));
        }
        if item.remote_ref == item.keeper_ref {
            return Err("candidate cannot keep itself".into());
        }
        if !seen.insert(&item.remote_ref) {
            return Err(format!("duplicate deletion candidate {}", item.remote_ref));
        }
        validate_oid(&item.expected_oid)?;
        validate_oid(&item.keeper_oid)?;
    }
    Ok(())
}

fn validate_branch(reference: &str) -> Result<(), String> {
    if !reference.starts_with("refs/heads/") {
        return Err("deletion API accepts branch refs only".into());
    }
    git_ok(args(["check-ref-format", reference]), "invalid branch ref")?;
    Ok(())
}

fn validate_oid(oid: &str) -> Result<(), String> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid expected object ID".into());
    }
    Ok(())
}

fn insert_expected(
    refs: &mut BTreeMap<String, String>,
    reference: &str,
    oid: &str,
) -> Result<(), String> {
    if let Some(old) = refs.insert(reference.to_owned(), oid.to_owned())
        && old != oid
    {
        return Err(format!("conflicting expected OIDs for {reference}"));
    }
    Ok(())
}

fn reject_url_rewrites() -> Result<(), String> {
    let output = git_output(args([
        "config",
        "--null",
        "--get-regexp",
        r"^url\..*\.(insteadOf|pushInsteadOf)$",
    ]))
    .map_err(|_| "could not inspect Git URL rewrites; refusing deletion".to_owned())?;
    if output.status.success() && !output.stdout.is_empty() {
        return Err("Git URL rewrites are configured; refusing remote mutation".into());
    }
    if !output.status.success() && output.status.code() != Some(1) {
        return Err("could not inspect Git URL rewrites; refusing deletion".into());
    }
    Ok(())
}

fn resolve_push_remote(remote: &str) -> Result<String, String> {
    if let Ok(bytes) = git_ok(
        args(["remote", "get-url", "--push", "--all", remote]),
        "cannot resolve configured remote",
    ) {
        let text = String::from_utf8_lossy(&bytes);
        let urls = text
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if urls.len() != 1 {
            return Err("configured remote must have exactly one push destination".into());
        }
        return Ok(urls[0].to_owned());
    }
    Ok(remote.to_owned())
}

fn default_branch(remote: &str) -> Result<String, String> {
    let output = git_ok(
        args(["ls-remote", "--symref", "--", remote, "HEAD"]),
        "cannot read remote default branch",
    )?;
    let mut found = None;
    for line in String::from_utf8_lossy(&output).lines() {
        if let Some((target, "HEAD")) = line.split_once('\t')
            && let Some(reference) = target.strip_prefix("ref: ")
            && reference.starts_with("refs/heads/")
        {
            if found.replace(reference.to_owned()).is_some() {
                return Err("remote advertised multiple default branches".into());
            }
        }
    }
    found.ok_or_else(|| "remote default branch is unknown; refusing deletion".into())
}

fn require_tips(remote: &str, expected: &BTreeMap<String, String>) -> Result<(), String> {
    let refs = expected.keys().map(String::as_str).collect::<Vec<_>>();
    for batch in refs.chunks(REF_BATCH) {
        let mut command = args(["ls-remote", "--refs", "--", remote]);
        command.extend(batch.iter().map(OsString::from));
        let actual = parse_refs(&git_ok(command, "cannot revalidate remote refs")?);
        for reference in batch {
            match (expected.get(*reference), actual.get(*reference)) {
                (Some(want), Some(got)) if want == got => {}
                (Some(_), Some(_)) => {
                    return Err(format!("remote ref {reference} changed since the plan"));
                }
                _ => return Err(format!("remote ref {reference} is missing")),
            }
        }
    }
    Ok(())
}

struct Isolated {
    _dir: tempfile::TempDir,
    git_dir: PathBuf,
}

impl Isolated {
    fn new() -> Result<Self, String> {
        let dir = tempfile::Builder::new()
            .prefix("find-recovery-delete-")
            .tempdir()
            .map_err(|e| format!("cannot create isolated verifier: {e}"))?;
        let git_dir = dir.path().join("objects.git");
        let hooks = dir.path().join("empty-hooks");
        std::fs::create_dir(&hooks).map_err(|e| format!("cannot prepare verifier: {e}"))?;
        let mut init = args(["init", "--bare", "-q", "--template"]);
        init.push(hooks.as_os_str().to_owned());
        init.push(git_dir.as_os_str().to_owned());
        git_ok(init, "cannot initialize isolated verifier")?;
        let mut config = args(["--git-dir"]);
        config.push(git_dir.as_os_str().to_owned());
        config.extend(args(["config", "core.hooksPath"]));
        config.push(hooks.as_os_str().to_owned());
        git_ok(config, "cannot disable local hooks")?;
        Ok(Self { _dir: dir, git_dir })
    }
}

fn verify_histories(repo: &Isolated, remote: &str, items: &[Candidate]) -> Result<(), String> {
    let mut refs = BTreeMap::new();
    for item in items {
        insert_expected(&mut refs, &item.remote_ref, &item.expected_oid)?;
        insert_expected(&mut refs, &item.keeper_ref, &item.keeper_oid)?;
    }
    let fetches = refs
        .iter()
        .enumerate()
        .map(|(i, (r, oid))| (r.clone(), oid.clone(), format!("refs/verify/{i:08}")))
        .collect::<Vec<_>>();
    for batch in fetches.chunks(REF_BATCH) {
        let mut command = args(["--git-dir"]);
        command.push(repo.git_dir.as_os_str().to_owned());
        command.extend(args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
        ]));
        command.push(OsString::from(remote));
        for (reference, _, destination) in batch {
            command.push(OsString::from(format!("+{reference}:{destination}")));
        }
        git_ok(
            command,
            "cannot fetch complete history into isolated verifier",
        )?;
    }
    for (_, expected, destination) in &fetches {
        let expression = format!("{destination}^{{commit}}");
        let mut command = args(["--git-dir"]);
        command.push(repo.git_dir.as_os_str().to_owned());
        command.extend(args(["rev-parse", "--verify", &expression]));
        if text(git_ok(command, "cannot verify fetched tip")?).trim() != expected {
            return Err("candidate or keeper changed during isolated fetch".into());
        }
    }
    let mut shallow = args(["--git-dir"]);
    shallow.push(repo.git_dir.as_os_str().to_owned());
    shallow.extend(args(["rev-parse", "--is-shallow-repository"]));
    if text(git_ok(shallow, "cannot inspect history completeness")?).trim() != "false" {
        return Err("isolated history is shallow; refusing deletion".into());
    }
    let mut graph_cmd = args(["--git-dir"]);
    graph_cmd.push(repo.git_dir.as_os_str().to_owned());
    graph_cmd.extend(args(["rev-list", "--parents", "--all"]));
    let graph = parent_graph(&git_ok(graph_cmd, "cannot read complete parent graph")?)?;
    for item in items {
        if !reachable(&graph, &item.expected_oid, &item.keeper_oid) {
            return Err(format!(
                "{} is not reachable from {}; refusing",
                item.remote_ref, item.keeper_ref
            ));
        }
    }
    Ok(())
}

fn parent_graph(bytes: &[u8]) -> Result<HashMap<String, Vec<String>>, String> {
    let mut graph = HashMap::new();
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let line =
            std::str::from_utf8(line).map_err(|_| "invalid commit graph encoding".to_owned())?;
        let mut fields = line.split_ascii_whitespace();
        let oid = fields.next().ok_or("empty commit graph record")?;
        graph.insert(oid.to_owned(), fields.map(str::to_owned).collect());
    }
    Ok(graph)
}

fn reachable(graph: &HashMap<String, Vec<String>>, ancestor: &str, tip: &str) -> bool {
    let mut todo = vec![tip];
    let mut seen = HashSet::new();
    while let Some(oid) = todo.pop() {
        if oid == ancestor {
            return true;
        }
        if seen.insert(oid)
            && let Some(parents) = graph.get(oid)
        {
            todo.extend(parents.iter().map(String::as_str));
        }
    }
    false
}

fn atomic_delete(repo: &Isolated, remote: &str, items: &[Candidate]) -> Result<(), String> {
    let keepers = items
        .iter()
        .map(|item| item.keeper_oid.as_str())
        .collect::<BTreeSet<_>>();
    let nonce = transaction_id();
    let anchors = keepers
        .iter()
        .enumerate()
        .map(|(i, oid)| (*oid, format!("{RETENTION}/{oid}/{nonce}-{i:08}")))
        .collect::<Vec<_>>();
    let estimated = remote.len()
        + items
            .iter()
            .map(|i| i.remote_ref.len() + i.expected_oid.len() + 96)
            .sum::<usize>()
        + anchors.iter().map(|(_, r)| r.len() + 96).sum::<usize>();
    if estimated > 512 * 1024 {
        return Err("atomic deletion plan exceeds safe argument size; refusing".into());
    }
    let mut command = args(["--git-dir"]);
    command.push(repo.git_dir.as_os_str().to_owned());
    command.extend(args([
        "push",
        "--porcelain",
        "--atomic",
        "--no-follow-tags",
        "--recurse-submodules=no",
    ]));
    for item in items {
        command.push(OsString::from(format!(
            "--force-with-lease={}:{}",
            item.remote_ref, item.expected_oid
        )));
    }
    for (_, anchor) in &anchors {
        command.push(OsString::from(format!("--force-with-lease={anchor}:")));
    }
    command.push(OsString::from("--"));
    command.push(OsString::from(remote));
    for (oid, anchor) in &anchors {
        command.push(OsString::from(format!("{oid}:{anchor}")));
    }
    for item in items {
        command.push(OsString::from(format!(":{}", item.remote_ref)));
    }
    let push = git_ok(
        command,
        "atomic deletion failed; no unconditional fallback was attempted",
    );
    let verify = verify_result(remote, items, &anchors);
    match (push, verify) {
        (Ok(_), Ok(())) | (Err(_), Ok(())) => Ok(()),
        (Err(error), Err(_)) => Err(error),
        (Ok(_), Err(error)) => Err(format!("push succeeded but postcondition failed: {error}")),
    }
}

fn verify_result(
    remote: &str,
    items: &[Candidate],
    anchors: &[(&str, String)],
) -> Result<(), String> {
    let expected = anchors
        .iter()
        .map(|(oid, r)| (r.clone(), (*oid).to_owned()))
        .collect::<BTreeMap<_, _>>();
    let actual = list_refs(remote, expected.keys().map(String::as_str))?;
    if expected.iter().any(|(r, oid)| actual.get(r) != Some(oid)) {
        return Err("retention tag is missing or changed".into());
    }
    let actual = list_refs(remote, items.iter().map(|item| item.remote_ref.as_str()))?;
    if let Some(item) = items
        .iter()
        .find(|item| actual.contains_key(&item.remote_ref))
    {
        return Err(format!("candidate {} still exists", item.remote_ref));
    }
    Ok(())
}

fn list_refs<'a>(
    remote: &str,
    refs: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, String>, String> {
    let refs = refs.into_iter().collect::<Vec<_>>();
    let mut output = BTreeMap::new();
    for batch in refs.chunks(REF_BATCH) {
        let mut command = args(["ls-remote", "--refs", "--", remote]);
        command.extend(batch.iter().map(OsString::from));
        output.extend(parse_refs(&git_ok(
            command,
            "cannot reconcile remote refs",
        )?));
    }
    Ok(output)
}

fn parse_refs(bytes: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(oid, reference)| (reference.to_owned(), oid.to_owned()))
        .collect()
}

fn transaction_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TRANSACTION_ID.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:032x}-{:08x}-{counter:08x}", std::process::id())
}

fn args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}
fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

fn git_output(args: Vec<OsString>) -> std::io::Result<Output> {
    let mut command = Command::new("git");
    command.args(args);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if matches!(
            name.as_ref(),
            "GIT_DIR"
                | "GIT_WORK_TREE"
                | "GIT_COMMON_DIR"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_INDEX_FILE"
                | "GIT_SHALLOW_FILE"
                | "GIT_GRAFT_FILE"
                | "GIT_REPLACE_REF_BASE"
        ) || name.starts_with("GIT_CONFIG_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command.output()
}

fn git_ok(args: Vec<OsString>, reason: &str) -> Result<Vec<u8>, String> {
    let output = git_output(args).map_err(|_| format!("{reason}: could not start Git"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!("{reason} ({})", output.status))
    }
}
