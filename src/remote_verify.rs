//! Batch verification that recovery commits are available from the remote alone.
//!
//! Verification uses a fresh bare repository for each call. It never borrows
//! the source clone's object database and fetches all requested refs in one
//! operation.

use std::{
    collections::{BTreeMap, BTreeSet},
    process::{Command, Output},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedRecord {
    /// Branch name without the `refs/heads/` prefix.
    pub remote_ref: String,
    /// Expected commit object ID.
    pub commit: String,
    /// Expected root tree ID for snapshot commits, if applicable.
    pub expected_tree: Option<String>,
}

/// Prove that every record can be fetched and validated from `remote` alone.
///
/// The complete remote-head listing is captured both before and after the
/// fetch. Any branch movement during verification fails the whole batch.
pub fn verify_saved_batch(remote: &str, records: &[SavedRecord]) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }
    if remote.is_empty()
        || remote.starts_with('-')
        || remote.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err("invalid remote argument".into());
    }
    validate_records(records)?;

    let before = remote_heads(remote)?;
    validate_expected_heads(&before, records)?;

    let temp = tempfile::tempdir().map_err(|e| format!("create isolated verifier: {e}"))?;
    let git_dir = temp.path().join("remote-only.git");
    let git_dir_s = git_dir.to_str().ok_or("non-UTF8 isolated verifier path")?;
    checked(
        isolated_git().args(["init", "--bare", "--quiet", git_dir_s]),
        "initialize isolated verifier",
    )?;
    checked(
        isolated_git().args(["--git-dir", git_dir_s, "remote", "add", "origin", remote]),
        "configure isolated verifier remote",
    )?;

    let mut fetch_args = vec![
        "--git-dir".to_owned(),
        git_dir_s.to_owned(),
        "-c".to_owned(),
        "fetch.fsckObjects=true".to_owned(),
        "fetch".to_owned(),
        "--no-tags".to_owned(),
        "--no-recurse-submodules".to_owned(),
        "origin".to_owned(),
    ];
    for (index, record) in records.iter().enumerate() {
        fetch_args.push(format!(
            "refs/heads/{}:refs/verify/saved/{index}",
            record.remote_ref
        ));
    }
    checked(
        isolated_git()
            .args(&fetch_args)
            .env("GIT_NO_LAZY_FETCH", "1"),
        "fetch recovery refs into isolated verifier",
    )?;

    let fetched = git_output(
        isolated_git().args([
            "--git-dir",
            git_dir_s,
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/verify/saved",
        ]),
        "list fetched recovery refs",
    )?;
    let fetched_refs = fetched
        .lines()
        .filter_map(|line| {
            let (oid, reference) = line.split_once(' ')?;
            Some((reference.to_owned(), oid.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    if fetched_refs.len() != records.len() {
        return Err("isolated fetch returned an unexpected set of refs".into());
    }
    let fetched_commits = records
        .iter()
        .map(|record| record.commit.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    for (index, record) in records.iter().enumerate() {
        let private_ref = format!("refs/verify/saved/{index}");
        let actual = fetched_refs.get(&private_ref);
        if actual.map(String::as_str) != Some(record.commit.as_str()) {
            return Err(format!(
                "fetched commit mismatch for refs/heads/{}: expected {}, got {}",
                record.remote_ref,
                record.commit,
                actual.map(String::as_str).unwrap_or("missing")
            ));
        }
    }

    ensure_no_alternates(&git_dir)?;

    let mut fsck = vec![
        "--git-dir".to_owned(),
        git_dir_s.to_owned(),
        "fsck".to_owned(),
        "--connectivity-only".to_owned(),
        "--no-reflogs".to_owned(),
    ];
    fsck.extend(fetched_commits.iter().map(|oid| (*oid).to_owned()));
    checked(
        isolated_git().args(&fsck).env("GIT_NO_LAZY_FETCH", "1"),
        "check isolated object connectivity",
    )?;

    let mut rev_list = vec![
        "--git-dir".to_owned(),
        git_dir_s.to_owned(),
        "rev-list".to_owned(),
        "--objects".to_owned(),
        "--missing=print".to_owned(),
    ];
    rev_list.extend(fetched_commits.iter().map(|oid| (*oid).to_owned()));
    let objects = git_output(
        isolated_git().args(&rev_list).env("GIT_NO_LAZY_FETCH", "1"),
        "enumerate isolated object closure",
    )?;
    if objects.lines().any(|line| line.starts_with('?')) {
        return Err("isolated remote fetch lacks one or more reachable objects".into());
    }

    let mut expected_trees = BTreeMap::new();
    for record in records {
        if let Some(tree) = &record.expected_tree {
            if expected_trees
                .insert(record.commit.as_str(), tree.as_str())
                .is_some_and(|previous| previous != tree)
            {
                return Err(format!(
                    "conflicting expected trees for commit {}",
                    record.commit
                ));
            }
        }
    }
    if !expected_trees.is_empty() {
        let expressions = expected_trees
            .keys()
            .map(|commit| format!("{commit}^{{tree}}"))
            .collect::<Vec<_>>();
        let tree_output = cat_file_batch_check(&git_dir, &expressions)?;
        if tree_output.len() != expressions.len() {
            return Err("cat-file returned an incomplete snapshot tree batch".into());
        }
        for ((commit, expected_tree), actual_tree) in expected_trees.iter().zip(tree_output) {
            if actual_tree != *expected_tree {
                let record = records
                    .iter()
                    .find(|record| record.commit == *commit)
                    .expect("expected tree came from a saved record");
                return Err(format!(
                    "snapshot tree mismatch for refs/heads/{}: expected {}, got {}",
                    record.remote_ref, expected_tree, actual_tree
                ));
            }
        }
    }

    let after = remote_heads(remote)?;
    validate_snapshot_stable(&before, &after)?;
    validate_expected_heads(&after, records)
}

fn cat_file_batch_check(
    git_dir: &std::path::Path,
    expressions: &[String],
) -> Result<Vec<String>, String> {
    let git_dir_s = git_dir.to_str().ok_or("non-UTF8 isolated verifier path")?;
    let mut child = isolated_git()
        .args([
            "--git-dir",
            git_dir_s,
            "cat-file",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("start snapshot tree batch check: {error}"))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().ok_or("cat-file stdin unavailable")?;
        for expression in expressions {
            writeln!(stdin, "{expression}")
                .map_err(|error| format!("write snapshot tree batch: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for snapshot tree batch: {error}"))?;
    if !output.status.success() {
        return Err(command_error("read snapshot tree batch", &output));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| format!("snapshot tree batch is not UTF-8: {error}"))?;
    let mut trees = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let oid = fields.next().ok_or("malformed cat-file tree row")?;
        let kind = fields.next().ok_or("malformed cat-file tree row")?;
        if kind != "tree" || !valid_oid(oid) {
            return Err("snapshot tree expression did not resolve to a tree".into());
        }
        trees.push(oid.to_owned());
    }
    Ok(trees)
}

fn validate_records(records: &[SavedRecord]) -> Result<(), String> {
    let mut refs = BTreeSet::new();
    for record in records {
        if !valid_branch_ref(&record.remote_ref) {
            return Err(format!(
                "unsafe or malformed remote branch: {}",
                record.remote_ref
            ));
        }
        if !refs.insert(&record.remote_ref) {
            return Err(format!(
                "duplicate remote branch in batch: {}",
                record.remote_ref
            ));
        }
        if !valid_oid(&record.commit) {
            return Err(format!(
                "malformed expected commit ID for {}",
                record.remote_ref
            ));
        }
        if record
            .expected_tree
            .as_deref()
            .is_some_and(|tree| !valid_oid(tree))
        {
            return Err(format!(
                "malformed expected tree ID for {}",
                record.remote_ref
            ));
        }
    }
    Ok(())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Validate Git's branch-ref restrictions without spawning one process per ref.
fn valid_branch_ref(name: &str) -> bool {
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
    {
        return false;
    }
    if name
        .bytes()
        .any(|byte| byte <= b' ' || byte == 0x7f || b"~^:?*[\\".contains(&byte))
    {
        return false;
    }
    name.split('/').all(|component| {
        !component.is_empty()
            && component != "@"
            && !component.starts_with('.')
            && !component.ends_with(".lock")
    })
}

fn validate_snapshot_stable(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Result<(), String> {
    if before == after {
        Ok(())
    } else {
        Err("remote branch listing changed during isolated verification".into())
    }
}

fn ensure_no_alternates(git_dir: &std::path::Path) -> Result<(), String> {
    if git_dir.join("objects/info/alternates").exists() {
        Err("isolated verifier unexpectedly has object alternates".into())
    } else {
        Ok(())
    }
}

fn validate_expected_heads(
    heads: &BTreeMap<String, String>,
    records: &[SavedRecord],
) -> Result<(), String> {
    for record in records {
        match heads.get(&record.remote_ref) {
            Some(actual) if actual == &record.commit => {}
            Some(actual) => {
                return Err(format!(
                    "remote ref moved: refs/heads/{} expected {}, got {}",
                    record.remote_ref, record.commit, actual
                ));
            }
            None => {
                return Err(format!(
                    "remote ref missing: refs/heads/{}",
                    record.remote_ref
                ));
            }
        }
    }
    Ok(())
}

fn remote_heads(remote: &str) -> Result<BTreeMap<String, String>, String> {
    let output = git_output(
        isolated_git().args(["ls-remote", "--heads", remote]),
        "list remote branches",
    )?;
    let mut heads = BTreeMap::new();
    for line in output.lines() {
        let (oid, full_ref) = line
            .split_once('\t')
            .ok_or_else(|| format!("malformed ls-remote row: {line}"))?;
        let name = full_ref
            .strip_prefix("refs/heads/")
            .ok_or_else(|| format!("unexpected remote ref: {full_ref}"))?;
        if !valid_oid(oid) || !valid_branch_ref(name) {
            return Err(format!("malformed remote head row: {line}"));
        }
        if heads.insert(name.to_owned(), oid.to_owned()).is_some() {
            return Err(format!("duplicate remote head row: {full_ref}"));
        }
    }
    Ok(heads)
}

fn checked(command: &mut Command, context: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("{context}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(context, &output))
    }
}

fn isolated_git() -> Command {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
    ] {
        command.env_remove(name);
    }
    for (name, _) in
        std::env::vars_os().filter(|(name, _)| name.to_string_lossy().starts_with("GIT_CONFIG_"))
    {
        command.env_remove(name);
    }
    command
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_output(command: &mut Command, context: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{context}: {error}"))?;
    if !output.status.success() {
        return Err(command_error(context, &output));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("{context}: invalid UTF-8: {error}"))
}

fn command_error(context: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{context}: {}", redact_url_credentials(stderr.trim()))
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
    use std::{fs, path::Path};

    struct Fixture {
        _root: tempfile::TempDir,
        remote: std::path::PathBuf,
        work: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let remote = root.path().join("remote.git");
            let work = root.path().join("work");
            run(&["git", "init", "--bare", "-q", path(&remote)]);
            run(&["git", "init", "-q", "-b", "main", path(&work)]);
            run(&[
                "git",
                "-C",
                path(&work),
                "config",
                "user.name",
                "Verify Test",
            ]);
            run(&[
                "git",
                "-C",
                path(&work),
                "config",
                "user.email",
                "verify-test@localhost",
            ]);
            Self {
                _root: root,
                remote,
                work,
            }
        }

        fn commit(&self, file: &str, content: &str, message: &str) -> (String, String) {
            fs::write(self.work.join(file), content).unwrap();
            run(&["git", "-C", path(&self.work), "add", file]);
            run(&["git", "-C", path(&self.work), "commit", "-qm", message]);
            let oid = output(&["git", "-C", path(&self.work), "rev-parse", "HEAD"]);
            let tree = output(&["git", "-C", path(&self.work), "rev-parse", "HEAD^{tree}"]);
            (oid, tree)
        }

        fn push(&self, oid: &str, name: &str) {
            run(&[
                "git",
                "-C",
                path(&self.work),
                "push",
                path(&self.remote),
                &format!("{oid}:refs/heads/{name}"),
            ]);
        }
    }

    fn path(value: &Path) -> &str {
        value.to_str().unwrap()
    }

    fn run(args: &[&str]) {
        let output = Command::new(args[0]).args(&args[1..]).output().unwrap();
        assert!(
            output.status.success(),
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn output(args: &[&str]) -> String {
        let result = Command::new(args[0]).args(&args[1..]).output().unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    }

    fn record(name: &str, commit: &str, tree: Option<&str>) -> SavedRecord {
        SavedRecord {
            remote_ref: name.into(),
            commit: commit.into(),
            expected_tree: tree.map(str::to_owned),
        }
    }

    #[test]
    fn verifies_shared_ancestry_and_duplicate_commit_refs_in_one_batch() {
        let fixture = Fixture::new();
        let (base, base_tree) = fixture.commit("a.txt", "base\n", "base");
        fixture.push(&base, "recovery/base");
        fixture.push(&base, "recovery/base-copy");
        let (tip, tip_tree) = fixture.commit("b.txt", "tip\n", "tip");
        fixture.push(&tip, "recovery/tip");

        verify_saved_batch(
            path(&fixture.remote),
            &[
                record("recovery/base", &base, Some(&base_tree)),
                record("recovery/base-copy", &base, Some(&base_tree)),
                record("recovery/tip", &tip, Some(&tip_tree)),
            ],
        )
        .unwrap();
    }

    #[test]
    fn missing_or_moved_remote_tip_fails_closed() {
        let fixture = Fixture::new();
        let (first, _) = fixture.commit("a.txt", "first\n", "first");
        fixture.push(&first, "recovery/one");
        let (second, _) = fixture.commit("a.txt", "second\n", "second");
        fixture.push(&second, "recovery/two");

        assert!(
            verify_saved_batch(
                path(&fixture.remote),
                &[
                    record("recovery/one", &first, None),
                    record("recovery/missing", &first, None),
                ]
            )
            .unwrap_err()
            .contains("missing")
        );
        assert!(
            verify_saved_batch(
                path(&fixture.remote),
                &[record("recovery/one", &second, None)]
            )
            .unwrap_err()
            .contains("moved")
        );
    }

    #[test]
    fn tree_mismatch_and_bad_records_fail_closed() {
        let fixture = Fixture::new();
        let (commit, tree) = fixture.commit("a.txt", "content\n", "snapshot");
        fixture.push(&commit, "recovery/snapshot");
        let wrong_tree = format!("{}{}", "0", &tree[1..]);
        assert!(
            verify_saved_batch(
                path(&fixture.remote),
                &[record("recovery/snapshot", &commit, Some(&wrong_tree))]
            )
            .unwrap_err()
            .contains("tree mismatch")
        );
        assert!(
            verify_saved_batch(
                path(&fixture.remote),
                &[record("recovery/../unsafe", &commit, None)]
            )
            .unwrap_err()
            .contains("unsafe")
        );
    }

    #[test]
    fn checks_ref_snapshot_movement_and_rejects_duplicate_names() {
        let fixture = Fixture::new();
        let (first, _) = fixture.commit("a.txt", "first\n", "first");
        let (second, _) = fixture.commit("a.txt", "second\n", "second");
        let before = BTreeMap::from([("recovery/x".into(), first.clone())]);
        let after = BTreeMap::from([("recovery/x".into(), second.clone())]);
        let saved = record("recovery/x", &first, None);
        assert!(
            validate_expected_heads(&after, std::slice::from_ref(&saved))
                .unwrap_err()
                .contains("moved")
        );
        assert!(
            validate_snapshot_stable(&before, &after)
                .unwrap_err()
                .contains("changed")
        );
        assert!(
            validate_records(&[
                record("recovery/x", &first, None),
                record("recovery/x", &first, None),
            ])
            .unwrap_err()
            .contains("duplicate")
        );
        assert!(!valid_oid(&"0".repeat(39)));
        assert!(!valid_branch_ref("refs/heads/bad..name"));
    }

    #[test]
    fn rejects_an_object_alternates_file_in_the_verifier() {
        let temp = tempfile::tempdir().unwrap();
        let git_dir = temp.path().join("repo.git");
        run(&["git", "init", "--bare", "-q", path(&git_dir)]);
        let alternates = git_dir.join("objects/info/alternates");
        fs::write(&alternates, "/some/other/object/store\n").unwrap();
        assert!(
            ensure_no_alternates(&git_dir)
                .unwrap_err()
                .contains("alternates")
        );
    }

    #[test]
    fn scrub_git_environment_and_redact_url_credentials() {
        let removed = isolated_git()
            .get_envs()
            .filter(|(name, value)| {
                matches!(
                    name.to_str(),
                    Some(
                        "GIT_DIR"
                            | "GIT_WORK_TREE"
                            | "GIT_COMMON_DIR"
                            | "GIT_INDEX_FILE"
                            | "GIT_OBJECT_DIRECTORY"
                            | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                            | "GIT_QUARANTINE_PATH"
                    )
                ) && value.is_none()
            })
            .count();
        assert_eq!(removed, 7);
        assert!(
            redact_url_credentials("fatal: https://alice:secret@example.invalid/repo.git denied")
                .contains("https://<redacted>@example.invalid/repo.git")
        );
    }
}
