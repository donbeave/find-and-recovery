//! Isolated, stable snapshot of remote branch tips for duplicate analysis.
//!
//! The temporary bare repository has its own object database. Inherited Git
//! object-directory overrides are stripped so a local clone cannot satisfy a
//! missing-object lookup. `blob:none` keeps history and trees available while
//! avoiding transfer of file contents; if the server rejects filtering, fetch
//! is retried without the filter.

use crate::dedupe::BranchSnapshot;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Fetch every remote head into a fresh isolated bare repository and return
/// its tip, tree, and reachable commit count for duplicate-tree groups.
///
/// Any remote movement during the fetch, malformed ref output, missing commit
/// or tree object, or Git failure aborts the snapshot.
pub fn fetch_remote_snapshots(remote: &str) -> Result<Vec<BranchSnapshot>, String> {
    let before = list_heads(remote)?;
    let temp = tempfile::Builder::new()
        .prefix("find-recovery-remote-snapshot-")
        .tempdir()
        .map_err(|error| format!("create isolated snapshot directory: {error}"))?;
    let git_dir = temp.path().join("objects.git");
    git_in(
        None,
        [
            OsStr::new("init"),
            OsStr::new("--bare"),
            OsStr::new("--quiet"),
            git_dir.as_os_str(),
        ],
    )?;

    let filtered = fetch_heads(remote, &git_dir, true);
    if filtered.is_err() {
        // Filter support is optional, especially for local and older servers.
        // Retry in the isolated repo without filtering; never use a local ODB.
        fetch_heads(remote, &git_dir, false)?;
    }

    let refs_output = git_in(
        Some(&git_dir),
        [
            OsStr::new("for-each-ref"),
            OsStr::new("--format=%(refname)%00%(objectname)"),
            OsStr::new("refs/remotes/snapshot"),
        ],
    )?;
    let fetched = parse_fetched_refs(&refs_output.stdout)?;
    let expected = before
        .iter()
        .map(|(name, oid)| (format!("refs/remotes/snapshot/{name}"), oid.clone()))
        .collect::<BTreeMap<_, _>>();
    if fetched != expected {
        return Err("fetched branch refs do not match the initial remote head listing".into());
    }

    let trees = batch_commit_trees(&git_dir, fetched.values())?;
    verify_tree_objects(&git_dir, trees.values())?;
    let mut by_tree: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (refname, oid) in &fetched {
        let branch = refname
            .strip_prefix("refs/remotes/snapshot/")
            .ok_or_else(|| format!("unexpected fetched ref name: {refname}"))?;
        let tree = trees
            .get(oid)
            .ok_or_else(|| format!("missing tree result for commit {oid}"))?
            .clone();
        by_tree
            .entry(tree)
            .or_default()
            .push((branch.to_owned(), oid.clone()));
    }

    let mut counts = BTreeMap::<String, u64>::new();
    let unique_oids = fetched.values().cloned().collect::<BTreeSet<_>>();
    for oid in unique_oids {
        let output = git_in(
            Some(&git_dir),
            [
                OsStr::new("rev-list"),
                OsStr::new("--count"),
                OsStr::new(&oid),
            ],
        )?;
        let count = String::from_utf8(output.stdout)
            .map_err(|_| format!("non-UTF-8 commit count for {oid}"))?
            .trim()
            .parse::<u64>()
            .map_err(|error| format!("invalid commit count for {oid}: {error}"))?;
        counts.insert(oid, count);
    }

    let mut snapshots = Vec::with_capacity(fetched.len());
    for (refname, oid) in fetched {
        let name = refname
            .strip_prefix("refs/remotes/snapshot/")
            .ok_or_else(|| format!("unexpected fetched ref name: {refname}"))?
            .to_owned();
        let tree_oid = trees
            .get(&oid)
            .ok_or_else(|| format!("missing tree result for commit {oid}"))?
            .clone();
        snapshots.push(BranchSnapshot {
            name,
            oid: oid.clone(),
            tree_oid,
            commit_count: *counts
                .get(&oid)
                .ok_or_else(|| format!("missing commit count for {oid}"))?,
        });
    }

    let after = list_heads(remote)?;
    if before != after {
        return Err("remote branch tips changed while taking snapshot".into());
    }
    Ok(snapshots)
}

/// Read the advertised default branch. If the remote omits symbolic HEAD,
/// return `None`; malformed or failed advertisements are errors.
pub fn default_branch(remote: &str) -> Result<Option<String>, String> {
    let mut command = Command::new("git");
    command.args([
        OsStr::new("ls-remote"),
        OsStr::new("--symref"),
        OsStr::new("--"),
        OsStr::new(remote),
        OsStr::new("HEAD"),
    ]);
    let output = checked(run(command, None)?, "read remote default branch")?;
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "remote symbolic HEAD is not UTF-8".to_owned())?;
    let mut default = None;
    for line in text.lines() {
        if let Some(target) = line.strip_prefix("ref: ") {
            let (target, label) = target
                .split_once('\t')
                .ok_or_else(|| format!("malformed remote symbolic HEAD: {line:?}"))?;
            if label != "HEAD" {
                return Err(format!("unexpected remote symbolic HEAD label: {label}"));
            }
            let branch = target
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .ok_or_else(|| format!("unexpected remote symbolic HEAD target: {target}"))?;
            if default.replace(branch.to_owned()).is_some() {
                return Err("remote advertised multiple default branches".into());
            }
        }
    }
    Ok(default)
}

fn fetch_heads(remote: &str, git_dir: &Path, filter_blobs: bool) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .arg("-c")
        .arg("protocol.file.allow=always")
        .arg("fetch")
        .arg("--quiet")
        .arg("--no-tags");
    if filter_blobs {
        command.arg("--filter=blob:none");
    }
    command
        .arg("--")
        .arg(remote)
        .arg("+refs/heads/*:refs/remotes/snapshot/*");
    run(command, None).and_then(|output| checked(output, "fetch remote heads"))
}

fn list_heads(remote: &str) -> Result<BTreeMap<String, String>, String> {
    let mut command = Command::new("git");
    command.args([
        OsStr::new("ls-remote"),
        OsStr::new("--heads"),
        OsStr::new("--"),
        OsStr::new(remote),
    ]);
    let output = checked(run(command, None)?, "list remote heads")?;
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "remote head listing is not UTF-8".to_owned())?;
    let mut heads = BTreeMap::new();
    for line in text.lines() {
        let (oid, refname) = line
            .split_once('\t')
            .ok_or_else(|| format!("malformed ls-remote line: {line:?}"))?;
        let branch = refname
            .strip_prefix("refs/heads/")
            .ok_or_else(|| format!("unexpected remote ref: {refname}"))?;
        if branch.is_empty()
            || oid.is_empty()
            || heads.insert(branch.to_owned(), oid.to_owned()).is_some()
        {
            return Err(format!(
                "invalid or duplicate remote branch entry: {line:?}"
            ));
        }
    }
    Ok(heads)
}

fn parse_fetched_refs(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "fetched refs are not UTF-8".to_owned())?;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (name, oid) = line
            .split_once('\0')
            .ok_or_else(|| format!("malformed fetched ref row: {line:?}"))?;
        if !name.starts_with("refs/remotes/snapshot/")
            || oid.is_empty()
            || result.insert(name.to_owned(), oid.to_owned()).is_some()
        {
            return Err(format!("invalid or duplicate fetched ref row: {line:?}"));
        }
    }
    Ok(result)
}

fn batch_commit_trees<'a>(
    git_dir: &Path,
    oids: impl Iterator<Item = &'a String>,
) -> Result<BTreeMap<String, String>, String> {
    let requested = oids
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if requested.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "--batch"]);
    let mut child = isolated(&mut command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start batched commit reader: {error}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or("batched commit reader stdin unavailable")?;
        for oid in &requested {
            writeln!(stdin, "{oid}")
                .map_err(|error| format!("write batched object request: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for batched commit reader: {error}"))?;
    let output = checked(output, "read fetched commit objects")?;
    let bytes = output.stdout;
    let mut cursor = 0usize;
    let mut result = BTreeMap::new();
    for expected_oid in requested {
        let header_end =
            find_byte(&bytes, cursor, b'\n').ok_or("truncated cat-file batch header")?;
        let header = std::str::from_utf8(&bytes[cursor..header_end])
            .map_err(|_| "cat-file header is not UTF-8".to_owned())?;
        let parts = header.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 3 || parts[0] != expected_oid || parts[1] != "commit" {
            return Err(format!(
                "expected commit {expected_oid}, got cat-file header {header:?}"
            ));
        }
        let size = parts[2]
            .parse::<usize>()
            .map_err(|error| format!("invalid commit object size: {error}"))?;
        let body_start = header_end + 1;
        let body_end = body_start
            .checked_add(size)
            .ok_or("commit object size overflow")?;
        if body_end >= bytes.len() || bytes[body_end] != b'\n' {
            return Err(format!("truncated commit object {expected_oid}"));
        }
        let body = &bytes[body_start..body_end];
        let tree_line = body
            .split(|byte| *byte == b'\n')
            .take_while(|line| !line.is_empty())
            .find(|line| line.starts_with(b"tree "))
            .ok_or_else(|| format!("commit {expected_oid} has no tree header"))?;
        let tree = std::str::from_utf8(&tree_line[5..])
            .map_err(|_| format!("tree OID is not UTF-8 in commit {expected_oid}"))?
            .to_owned();
        if tree.is_empty() || result.insert(expected_oid.clone(), tree).is_some() {
            return Err(format!(
                "invalid or duplicate commit result for {expected_oid}"
            ));
        }
        cursor = body_end + 1;
    }
    if cursor != bytes.len() {
        return Err("unexpected trailing bytes from cat-file batch".into());
    }
    Ok(result)
}

fn verify_tree_objects<'a>(
    git_dir: &Path,
    trees: impl Iterator<Item = &'a String>,
) -> Result<(), String> {
    let requested = trees.cloned().collect::<BTreeSet<_>>();
    if requested.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]);
    let input = requested
        .iter()
        .map(|oid| format!("{oid}\n"))
        .collect::<String>();
    let mut child = isolated(&mut command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start batched tree verifier: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("batched tree verifier stdin unavailable")?
        .write_all(input.as_bytes())
        .map_err(|error| format!("write batched tree requests: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for batched tree verifier: {error}"))?;
    let output = checked(output, "verify fetched tree objects")?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "tree verifier output is not UTF-8".to_owned())?;
    let rows = text.lines().collect::<Vec<_>>();
    if rows.len() != requested.len() {
        return Err("tree verifier returned an incomplete result".into());
    }
    for (row, expected_oid) in rows.into_iter().zip(requested) {
        let fields = row.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[0] != expected_oid || fields[1] != "tree" {
            return Err(format!("expected tree object {expected_oid}, got {row:?}"));
        }
    }
    Ok(())
}

fn find_byte(bytes: &[u8], start: usize, needle: u8) -> Option<usize> {
    bytes
        .get(start..)?
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| start + offset)
}

fn git_in<const N: usize>(git_dir: Option<&Path>, args: [&OsStr; N]) -> Result<Output, String> {
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    command.args(args);
    checked(run(command, None)?, "run git command")
}

fn run(mut command: Command, input: Option<&[u8]>) -> Result<Output, String> {
    if let Some(input) = input {
        command.stdin(Stdio::piped());
        let mut child = isolated(&mut command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("start git: {error}"))?;
        child
            .stdin
            .as_mut()
            .ok_or("git stdin unavailable")?
            .write_all(input)
            .map_err(|error| format!("write git stdin: {error}"))?;
        return child
            .wait_with_output()
            .map_err(|error| format!("wait for git: {error}"));
    }
    isolated(&mut command)
        .output()
        .map_err(|error| format!("run git: {error}"))
}

fn isolated(command: &mut Command) -> &mut Command {
    // Keep SSH/authentication settings intact; remove variables that can
    // redirect object lookup, repository selection, or Git configuration.
    strip_git_config_environment(command, std::env::vars_os().map(|(name, _)| name));
    for name in [
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_NO_LAZY_FETCH",
    ] {
        command.env_remove(name);
    }
    command.env("GIT_NO_LAZY_FETCH", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn strip_git_config_environment(
    command: &mut Command,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) {
    // Always clear singleton overrides, including ones added directly to a
    // Command before this helper is called. Remove every inherited indexed
    // KEY_n / VALUE_n override as well.
    for name in [
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
    ] {
        command.env_remove(name);
    }
    for name in names {
        if name.to_string_lossy().starts_with("GIT_CONFIG_") {
            command.env_remove(name);
        }
    }
}

fn checked(output: Output, operation: &str) -> Result<Output, String> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{operation} failed ({}): {}",
            output.status,
            redact_url_credentials(String::from_utf8_lossy(&output.stderr).trim())
        ))
    }
}

fn redact_url_credentials(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(scheme_end) = rest.find("://") {
        let authority_start = scheme_end + 3;
        output.push_str(&rest[..authority_start]);
        let authority_end = rest[authority_start..]
            .find(['/', '?', '#', ' ', '\t', '\r', '\n'])
            .map(|offset| authority_start + offset)
            .unwrap_or(rest.len());
        let authority = &rest[authority_start..authority_end];
        if let Some(at) = authority.rfind('@') {
            output.push_str("<redacted>@");
            output.push_str(&authority[at + 1..]);
        } else {
            output.push_str(authority);
        }
        rest = &rest[authority_end..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        remote: std::path::PathBuf,
        work: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let remote = dir.path().join("remote.git");
            let work = dir.path().join("work");
            test_git(
                None,
                ["init", "--bare", "--quiet", remote.to_str().unwrap()],
            );
            test_git(None, ["init", "--quiet", work.to_str().unwrap()]);
            test_git(Some(&work), ["config", "user.name", "Snapshot Test"]);
            test_git(
                Some(&work),
                ["config", "user.email", "snapshot@example.invalid"],
            );
            std::fs::write(work.join("content"), "same tree\n").unwrap();
            test_git(Some(&work), ["add", "content"]);
            test_git(Some(&work), ["commit", "--quiet", "-m", "base"]);
            Self {
                _dir: dir,
                remote,
                work,
            }
        }

        fn publish(&self) {
            test_git(
                Some(&self.work),
                ["push", "--quiet", self.remote.to_str().unwrap(), "--all"],
            );
        }

        fn branch_at_current(&self, name: &str, message: &str) {
            test_git(Some(&self.work), ["checkout", "--quiet", "-b", name]);
            test_git(
                Some(&self.work),
                ["commit", "--quiet", "--allow-empty", "-m", message],
            );
        }
    }

    #[test]
    fn discovers_advertised_default_branch() {
        let fixture = Fixture::new();
        test_git(
            Some(&fixture.work),
            [
                "push",
                "--quiet",
                fixture.remote.to_str().unwrap(),
                "HEAD:refs/heads/stable",
            ],
        );
        test_git(
            None,
            [
                "--git-dir",
                fixture.remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/stable",
            ],
        );
        assert_eq!(
            default_branch(fixture.remote.to_str().unwrap()).unwrap(),
            Some("stable".into())
        );
    }

    fn test_git<const N: usize>(cwd: Option<&Path>, args: [&str; N]) -> Output {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    #[test]
    fn reads_different_commit_ids_with_identical_tree() {
        let fixture = Fixture::new();
        let base = String::from_utf8(test_git(Some(&fixture.work), ["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        fixture.branch_at_current("recovery/older", "older empty commit");
        test_git(Some(&fixture.work), ["checkout", "--quiet", base.as_str()]);
        fixture.branch_at_current("release/newer", "newer empty commit");
        fixture.branch_at_current("release/tip", "newest empty commit");
        fixture.publish();

        let snapshots = fetch_remote_snapshots(fixture.remote.to_str().unwrap()).unwrap();
        let older = snapshots
            .iter()
            .find(|branch| branch.name == "recovery/older")
            .unwrap();
        let newer = snapshots
            .iter()
            .find(|branch| branch.name == "release/tip")
            .unwrap();
        assert_ne!(older.oid, newer.oid);
        assert_eq!(older.tree_oid, newer.tree_oid);
        // Higher reachable count is independent of ancestry containment.
        assert_eq!(older.commit_count, 2);
        assert_eq!(newer.commit_count, 3);
    }

    #[test]
    fn divergent_same_tree_branches_and_slashes_are_preserved() {
        let fixture = Fixture::new();
        let base = String::from_utf8(test_git(Some(&fixture.work), ["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        fixture.branch_at_current("topic/one", "first empty commit");
        test_git(Some(&fixture.work), ["checkout", "--quiet", base.as_str()]);
        fixture.branch_at_current("topic/two", "second empty commit");
        fixture.publish();

        let snapshots = fetch_remote_snapshots(fixture.remote.to_str().unwrap()).unwrap();
        let one = snapshots
            .iter()
            .find(|branch| branch.name == "topic/one")
            .unwrap();
        let two = snapshots
            .iter()
            .find(|branch| branch.name == "topic/two")
            .unwrap();
        assert_eq!(one.tree_oid, two.tree_oid);
        assert_eq!(one.commit_count, two.commit_count);
        assert_eq!(one.name, "topic/one");
        assert_eq!(two.name, "topic/two");
    }

    #[test]
    fn unique_tree_branch_has_accurate_reachable_count() {
        let fixture = Fixture::new();
        fixture.publish();
        let snapshots = fetch_remote_snapshots(fixture.remote.to_str().unwrap()).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].commit_count, 1);
    }

    #[test]
    fn exact_same_tip_refs_share_one_batched_object_result() {
        let fixture = Fixture::new();
        test_git(Some(&fixture.work), ["branch", "topic/alias"]);
        fixture.publish();
        let snapshots = fetch_remote_snapshots(fixture.remote.to_str().unwrap()).unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].oid, snapshots[1].oid);
        assert_eq!(snapshots[0].tree_oid, snapshots[1].tree_oid);
        assert_eq!(snapshots[0].commit_count, 1);
        assert_eq!(snapshots[1].commit_count, 1);
    }

    #[test]
    fn reachable_commit_counts_rank_linear_same_tree_tips() {
        let fixture = Fixture::new();
        let base = String::from_utf8(test_git(Some(&fixture.work), ["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        fixture.branch_at_current("topic/short", "short empty commit");
        test_git(
            Some(&fixture.work),
            ["checkout", "--quiet", "-b", "topic/mid"],
        );
        test_git(
            Some(&fixture.work),
            [
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "middle empty commit",
            ],
        );
        test_git(
            Some(&fixture.work),
            ["checkout", "--quiet", "-b", "topic/long"],
        );
        test_git(
            Some(&fixture.work),
            [
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "longest empty commit",
            ],
        );
        test_git(Some(&fixture.work), ["checkout", "--quiet", base.as_str()]);
        fixture.publish();

        let snapshots = fetch_remote_snapshots(fixture.remote.to_str().unwrap()).unwrap();
        let short = snapshots
            .iter()
            .find(|branch| branch.name == "topic/short")
            .unwrap();
        let long = snapshots
            .iter()
            .find(|branch| branch.name == "topic/long")
            .unwrap();
        assert_eq!(short.tree_oid, long.tree_oid);
        let mid = snapshots
            .iter()
            .find(|branch| branch.name == "topic/mid")
            .unwrap();
        assert_eq!(short.commit_count + 2, long.commit_count);
        assert_eq!(short.commit_count + 1, mid.commit_count);
    }

    #[test]
    fn poisoned_git_config_environment_is_ignored_for_local_remote() {
        let fixture = Fixture::new();
        fixture.publish();
        let remote = fixture.remote.to_string_lossy().into_owned();
        let poison_url = "https://poison-user:poison-secret@127.0.0.1:1/poison/";
        let key = format!("url.{poison_url}.insteadOf");
        let config = format!("[url \"{poison_url}\"]\n\tinsteadOf = {remote}\n");
        let config_path = fixture._dir.path().join("poison.gitconfig");
        std::fs::write(&config_path, &config).unwrap();
        let config_path = config_path.to_string_lossy().into_owned();

        // Prove this config would redirect the local remote if it reached Git.
        let poisoned = Command::new("git")
            .args(["ls-remote", "--heads", "--", &remote])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", &key)
            .env("GIT_CONFIG_VALUE_0", &remote)
            .output()
            .unwrap();
        assert!(!poisoned.status.success());

        let parameters = format!("'{key}={remote}'");
        let mut command = Command::new("git");
        command
            .args(["ls-remote", "--heads", "--", &remote])
            .env("GIT_CONFIG", &config_path)
            .env("GIT_CONFIG_GLOBAL", &config_path)
            .env("GIT_CONFIG_SYSTEM", &config_path)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", &key)
            .env("GIT_CONFIG_VALUE_0", &remote)
            .env("GIT_CONFIG_PARAMETERS", parameters);
        strip_git_config_environment(
            &mut command,
            [
                "GIT_CONFIG".into(),
                "GIT_CONFIG_GLOBAL".into(),
                "GIT_CONFIG_SYSTEM".into(),
                "GIT_CONFIG_COUNT".into(),
                "GIT_CONFIG_PARAMETERS".into(),
                "GIT_CONFIG_KEY_0".into(),
                "GIT_CONFIG_VALUE_0".into(),
            ],
        );
        let sanitized = command.output().unwrap();
        assert!(
            sanitized.status.success(),
            "sanitized local ls-remote failed: {}",
            String::from_utf8_lossy(&sanitized.stderr)
        );
        assert!(!sanitized.stdout.is_empty());
    }

    #[test]
    fn remote_failure_diagnostics_redact_embedded_credentials() {
        let sample =
            redact_url_credentials("fatal: https://alice:secret@example.invalid/repo.git: denied");
        assert!(sample.contains("https://<redacted>@example.invalid/repo.git"));
        assert!(!sample.contains("alice"));
        assert!(!sample.contains("secret"));

        let error =
            fetch_remote_snapshots("https://alice:secret@127.0.0.1:1/private.git").unwrap_err();
        assert!(!error.contains("alice"));
        assert!(!error.contains("secret"));
    }
}
