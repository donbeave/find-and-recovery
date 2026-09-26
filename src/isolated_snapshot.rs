//! Read-only inventory of the staged index and current worktree as Git trees.
//!
//! Git writes objects while building trees. This module directs those writes
//! into a temporary object database and exposes the repository object database
//! only as a read-only alternate. Temporary objects disappear when the call
//! returns, so callers that later preserve a snapshot must materialize it in
//! their own explicit save step.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Tree IDs for the staged index and the current tracked plus non-ignored
/// worktree contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeTreeSnapshot {
    pub staged_tree_oid: String,
    pub worktree_tree_oid: String,
}

/// Compute staged-index and worktree tree IDs without writing generated
/// objects, refs, or index changes into the source repository.
///
/// Custom clean filters are rejected before Git adds worktree files. Their
/// commands can have arbitrary side effects, and their preservation semantics
/// are not modeled here.
pub fn snapshot_worktree(repo: &Path) -> Result<WorktreeTreeSnapshot, String> {
    let root = git_path(repo, ["rev-parse", "--show-toplevel"], "locate worktree")?;
    let object_dir = git_path(
        &root,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
        "locate repository object database",
    )?;
    let index_path = git_path(
        &root,
        ["rev-parse", "--path-format=absolute", "--git-path", "index"],
        "locate repository index",
    )?;
    if !object_dir.is_dir() {
        return Err("repository object database is missing or not a directory".into());
    }
    let index_exists = regular_file_exists(&index_path, "repository index")?;

    let shared_index = shared_index_path(&root)?;
    let head_tree = head_tree(&root)?;
    let candidate_paths = worktree_paths(&root)?;
    reject_custom_filters(&root, &candidate_paths, index_exists)?;

    let temp = tempfile::Builder::new()
        .prefix("find-recovery-local-snapshot-")
        .tempdir()
        .map_err(|error| format!("create isolated snapshot directory: {error}"))?;
    let isolated_objects = temp.path().join("objects");
    fs::create_dir(&isolated_objects)
        .map_err(|error| format!("create isolated object database: {error}"))?;

    let staged_index = temp.path().join("staged-index");
    if index_exists {
        fs::copy(&index_path, &staged_index)
            .map_err(|error| format!("copy repository index for snapshot: {error}"))?;
        if let Some(shared_index) = shared_index {
            let name = shared_index
                .file_name()
                .ok_or("shared index path has no filename")?
                .to_os_string();
            fs::copy(&shared_index, temp.path().join(name))
                .map_err(|error| format!("copy shared repository index for snapshot: {error}"))?;
        }
    }

    let object_store = IsolatedObjectStore {
        objects: &isolated_objects,
        source_objects: &object_dir,
    };
    if !index_exists {
        let read_tree_args = head_tree
            .as_deref()
            .map(|tree| vec![OsString::from("read-tree"), OsString::from(tree)])
            .unwrap_or_else(|| vec![OsString::from("read-tree"), OsString::from("--empty")]);
        git_checked(
            &root,
            read_tree_args,
            Some(&staged_index),
            Some(&object_store),
            None,
            "initialize isolated staged index",
        )?;
    }
    // `write-tree` can reuse a tree already present in the object database
    // without opening every blob named by the index. Validate each index entry
    // first so a dangling/corrupt index cannot be reported as a preserved
    // snapshot merely because its tree object happens to exist.
    validate_index_objects(&root, &staged_index, &object_store)?;
    let staged_tree_oid = git_stdout(
        &root,
        ["write-tree"],
        Some(&staged_index),
        Some(&object_store),
        None,
        "write isolated staged tree",
    )?;

    let worktree_index = temp.path().join("worktree-index");
    if let Some(tree) = head_tree.as_deref() {
        git_checked(
            &root,
            ["read-tree", tree],
            Some(&worktree_index),
            Some(&object_store),
            None,
            "initialize isolated worktree index from HEAD",
        )?;
    } else {
        git_checked(
            &root,
            ["read-tree", "--empty"],
            Some(&worktree_index),
            Some(&object_store),
            None,
            "initialize isolated worktree index",
        )?;
    }
    git_checked(
        &root,
        ["add", "-A", "--", "."],
        Some(&worktree_index),
        Some(&object_store),
        None,
        "add worktree contents to isolated index",
    )?;
    let worktree_tree_oid = git_stdout(
        &root,
        ["write-tree"],
        Some(&worktree_index),
        Some(&object_store),
        None,
        "write isolated worktree tree",
    )?;

    Ok(WorktreeTreeSnapshot {
        staged_tree_oid,
        worktree_tree_oid,
    })
}

fn validate_index_objects(
    repo: &Path,
    index: &Path,
    object_store: &IsolatedObjectStore<'_>,
) -> Result<(), String> {
    let output = git_checked(
        repo,
        ["ls-files", "--stage", "-z"],
        Some(index),
        Some(object_store),
        None,
        "inspect isolated staged index objects",
    )?;
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(separator) = entry.iter().position(|byte| *byte == b'\t') else {
            return Err("write isolated staged tree failed: malformed staged index entry".into());
        };
        let metadata = std::str::from_utf8(&entry[..separator])
            .map_err(|_| "write isolated staged tree failed: non-UTF-8 index metadata")?;
        let mut fields = metadata.split_ascii_whitespace();
        let mode = fields
            .next()
            .ok_or("write isolated staged tree failed: missing index mode")?;
        let oid = fields
            .next()
            .ok_or("write isolated staged tree failed: missing index object ID")?;
        if fields.next().is_none() || fields.next().is_some() {
            return Err(
                "write isolated staged tree failed: malformed staged index metadata".into(),
            );
        }
        let expected_type = if mode == "160000" { "commit" } else { "blob" };
        let actual_type = git_stdout(
            repo,
            ["cat-file", "-t", oid],
            Some(index),
            Some(object_store),
            None,
            "write isolated staged tree",
        )?;
        if actual_type != expected_type {
            return Err(format!(
                "write isolated staged tree failed: index object {oid} has type {actual_type}, expected {expected_type}"
            ));
        }
    }
    Ok(())
}

struct IsolatedObjectStore<'a> {
    objects: &'a Path,
    source_objects: &'a Path,
}

fn worktree_paths(repo: &Path) -> Result<Vec<u8>, String> {
    let tracked = git_checked(
        repo,
        ["ls-files", "--cached", "-z"],
        None,
        None,
        None,
        "list tracked paths for snapshot",
    )?;
    let untracked = git_checked(
        repo,
        ["ls-files", "--others", "--exclude-standard", "-z"],
        None,
        None,
        None,
        "list untracked paths for snapshot",
    )?;
    let mut paths = BTreeSet::new();
    for output in [tracked.stdout, untracked.stdout] {
        for path in parse_nul_paths(&output, "Git path listing")? {
            paths.insert(path);
        }
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(&path);
        input.push(0);
    }
    Ok(input)
}

fn reject_custom_filters(repo: &Path, paths: &[u8], has_index: bool) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let worktree_attrs = git_checked(
        repo,
        ["check-attr", "-z", "--stdin", "filter"],
        None,
        None,
        Some(paths),
        "inspect worktree clean-filter attributes",
    )?;
    reject_filter_records(&worktree_attrs.stdout, paths)?;

    if has_index {
        let index_attrs = git_checked(
            repo,
            ["check-attr", "--cached", "-z", "--stdin", "filter"],
            None,
            None,
            Some(paths),
            "inspect index clean-filter attributes",
        )?;
        reject_filter_records(&index_attrs.stdout, paths)?;
    }
    Ok(())
}

fn reject_filter_records(output: &[u8], paths: &[u8]) -> Result<(), String> {
    let records = parse_nul_fields(output, "Git attribute output")?;
    let expected = parse_nul_paths(paths, "snapshot path list")?;
    if records.len() != expected.len() * 3 {
        return Err("Git returned an incomplete clean-filter attribute listing".into());
    }
    for record in records.chunks_exact(3) {
        if record[1] != b"filter" {
            return Err("Git returned an unexpected attribute while checking clean filters".into());
        }
        if record[2] != b"unspecified" && record[2] != b"unset" {
            return Err(format!(
                "unsupported clean filter attribute {:?} on {}; refusing worktree snapshot",
                String::from_utf8_lossy(record[2]),
                String::from_utf8_lossy(record[0])
            ));
        }
    }
    Ok(())
}

fn parse_nul_paths(output: &[u8], context: &str) -> Result<Vec<Vec<u8>>, String> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        return Err(format!("{context} is not NUL terminated"));
    }
    let paths = output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if paths.iter().any(Vec::is_empty) {
        return Err(format!("{context} contains an empty path"));
    }
    Ok(paths)
}

fn parse_nul_fields<'a>(output: &'a [u8], context: &str) -> Result<Vec<&'a [u8]>, String> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        return Err(format!("{context} is not NUL terminated"));
    }
    Ok(output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .collect())
}

fn shared_index_path(repo: &Path) -> Result<Option<PathBuf>, String> {
    let output = git_checked(
        repo,
        ["rev-parse", "--shared-index-path"],
        None,
        None,
        None,
        "locate shared index",
    )?;
    let value = output_line(&output.stdout, "shared index path")?;
    if value.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    Ok(Some(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }))
}

fn regular_file_exists(path: &Path, context: &str) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(format!("{context} is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect {context}: {error}")),
    }
}

fn head_tree(repo: &Path) -> Result<Option<String>, String> {
    let resolved = git_raw(
        repo,
        ["rev-parse", "--verify", "--quiet", "HEAD^{tree}"],
        None,
        None,
        None,
    )?;
    if resolved.status.success() {
        return Ok(Some(
            output_line(&resolved.stdout, "HEAD tree ID")?.to_owned(),
        ));
    }

    let symbolic = git_raw(repo, ["symbolic-ref", "--quiet", "HEAD"], None, None, None)?;
    if !symbolic.status.success() {
        return Err("cannot resolve HEAD tree for worktree snapshot".into());
    }
    let reference = output_line(&symbolic.stdout, "HEAD symbolic reference")?;
    let exists = git_raw(
        repo,
        ["show-ref", "--verify", "--quiet", reference],
        None,
        None,
        None,
    )?;
    if exists.status.code() == Some(1) {
        return Ok(None);
    }
    if exists.status.success() {
        return Err("HEAD reference exists but its tree cannot be resolved".into());
    }
    Err("cannot inspect HEAD reference for worktree snapshot".into())
}

fn git_path<I, S>(repo: &Path, args: I, context: &str) -> Result<PathBuf, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_checked(repo, args, None, None, None, context)?;
    let path = output_line(&output.stdout, context)?;
    if path.is_empty() {
        return Err(format!("{context}: Git returned an empty path"));
    }
    Ok(PathBuf::from(path))
}

fn git_stdout<I, S>(
    repo: &Path,
    args: I,
    index: Option<&Path>,
    object_store: Option<&IsolatedObjectStore<'_>>,
    input: Option<&[u8]>,
    context: &str,
) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_checked(repo, args, index, object_store, input, context)?;
    let value = output_line(&output.stdout, context)?;
    if value.is_empty() {
        return Err(format!("{context}: Git returned an empty object ID"));
    }
    Ok(value.to_owned())
}

fn output_line<'a>(output: &'a [u8], context: &str) -> Result<&'a str, String> {
    let output = output.strip_suffix(b"\n").unwrap_or(output);
    let output = output.strip_suffix(b"\r").unwrap_or(output);
    std::str::from_utf8(output).map_err(|_| format!("{context}: Git returned non-UTF-8 output"))
}

fn git_checked<I, S>(
    repo: &Path,
    args: I,
    index: Option<&Path>,
    object_store: Option<&IsolatedObjectStore<'_>>,
    input: Option<&[u8]>,
    context: &str,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_raw(repo, args, index, object_store, input)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{context} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn git_raw<I, S>(
    repo: &Path,
    args: I,
    index: Option<&Path>,
    object_store: Option<&IsolatedObjectStore<'_>>,
    input: Option<&[u8]>,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg("core.fsmonitor=false")
        .args(args);
    sanitize_git_environment(&mut command);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    if let Some(object_store) = object_store {
        command
            .env("GIT_OBJECT_DIRECTORY", object_store.objects)
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                alternate_object_path(object_store.source_objects)?,
            );
    }
    if let Some(input) = input {
        command.stdin(Stdio::piped());
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("start git: {error}"))?;
        let mut stdin = child.stdin.take().ok_or("git stdin unavailable")?;
        let input = input.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for git: {error}"))?;
        writer
            .join()
            .map_err(|_| "git input writer thread panicked")?
            .map_err(|error| format!("write git input: {error}"))?;
        Ok(output)
    } else {
        command
            .output()
            .map_err(|error| format!("run git: {error}"))
    }
}

fn sanitize_git_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
}

fn alternate_object_path(path: &Path) -> Result<OsString, String> {
    let path = path
        .to_str()
        .ok_or("repository object path is not UTF-8; refusing isolated snapshot")?;
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for character in path.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(OsString::from(quoted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use walkdir::WalkDir;

    struct Fixture {
        _root: TempDir,
        repo: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = tempfile::Builder::new().prefix(label).tempdir().unwrap();
            let repo = root.path().join("repo");
            fs::create_dir(&repo).unwrap();
            git(&repo, ["init", "-q", "-b", "main"]);
            git(&repo, ["config", "user.name", "Snapshot Test"]);
            git(&repo, ["config", "user.email", "snapshot-test@localhost"]);
            fs::write(repo.join("tracked.txt"), "base\n").unwrap();
            git(&repo, ["add", "tracked.txt"]);
            git(&repo, ["commit", "-qm", "base"]);
            Self { _root: root, repo }
        }

        fn dirty(&self) {
            fs::write(self.repo.join("tracked.txt"), "staged content\n").unwrap();
            fs::write(self.repo.join("staged-only.txt"), "staged file\n").unwrap();
            git(&self.repo, ["add", "tracked.txt", "staged-only.txt"]);
            fs::write(self.repo.join("tracked.txt"), "unstaged content\n").unwrap();
            fs::write(self.repo.join("untracked.txt"), "untracked file\n").unwrap();
        }
    }

    #[test]
    fn computes_staged_and_unstaged_trees_without_touching_source_state() {
        let source = Fixture::new("find-recovery-isolated-source-");
        let oracle = Fixture::new("find-recovery-isolated-oracle-");
        git(&source.repo, ["update-index", "--split-index"]);
        source.dirty();
        oracle.dirty();

        let staged_expected = git_output(&oracle.repo, ["write-tree"]);
        let oracle_index = oracle._root.path().join("oracle-worktree-index");
        git_with_index(&oracle.repo, &oracle_index, ["read-tree", "HEAD"]);
        git_with_index(&oracle.repo, &oracle_index, ["add", "-A", "--", "."]);
        let worktree_expected = git_with_index(&oracle.repo, &oracle_index, ["write-tree"]);
        assert_ne!(staged_expected, worktree_expected);

        let before = repo_state(&source.repo);
        let actual = snapshot_worktree(&source.repo).unwrap();
        let after = repo_state(&source.repo);

        assert_eq!(actual.staged_tree_oid, staged_expected);
        assert_eq!(actual.worktree_tree_oid, worktree_expected);
        assert_eq!(
            after.object_store, before.object_store,
            "source object store changed"
        );
        assert_eq!(after.refs, before.refs, "source refs changed");
        assert_eq!(after.head, before.head, "source HEAD changed");
        assert_eq!(after.index, before.index, "source index changed");
        assert_eq!(after.worktree, before.worktree, "source worktree changed");
    }

    #[test]
    fn refuses_repository_clean_filter_without_running_it() {
        let fixture = Fixture::new("find-recovery-isolated-filter-");
        let marker = fixture._root.path().join("filter-ran");
        let filter = fixture._root.path().join("clean-filter.sh");
        fs::write(
            &filter,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&filter).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&filter, permissions).unwrap();
        git(
            &fixture.repo,
            ["config", "filter.probe.clean", filter.to_str().unwrap()],
        );
        fs::write(fixture.repo.join(".gitattributes"), "*.txt filter=probe\n").unwrap();
        fs::write(fixture.repo.join("tracked.txt"), "changed\n").unwrap();

        let before = repo_state(&fixture.repo);
        let error = snapshot_worktree(&fixture.repo).unwrap_err();
        let after = repo_state(&fixture.repo);

        assert!(error.contains("unsupported clean filter attribute"));
        assert!(!marker.exists(), "clean-filter command unexpectedly ran");
        assert_eq!(
            after.object_store, before.object_store,
            "source object store changed"
        );
        assert_eq!(after.refs, before.refs, "source refs changed");
        assert_eq!(after.head, before.head, "source HEAD changed");
        assert_eq!(after.index, before.index, "source index changed");
        assert_eq!(after.worktree, before.worktree, "source worktree changed");
    }

    #[test]
    fn refuses_clean_filter_present_only_in_staged_attributes() {
        let fixture = Fixture::new("find-recovery-isolated-index-filter-");
        git(&fixture.repo, ["config", "filter.probe.clean", "cat"]);
        fs::write(fixture.repo.join(".gitattributes"), "*.txt filter=probe\n").unwrap();
        git(&fixture.repo, ["add", ".gitattributes"]);
        fs::write(fixture.repo.join(".gitattributes"), "").unwrap();
        fs::write(fixture.repo.join("tracked.txt"), "changed\n").unwrap();

        let before = repo_state(&fixture.repo);
        let error = snapshot_worktree(&fixture.repo).unwrap_err();
        let after = repo_state(&fixture.repo);

        assert!(error.contains("unsupported clean filter attribute"));
        assert_eq!(
            after.object_store, before.object_store,
            "source object store changed"
        );
        assert_eq!(after.refs, before.refs, "source refs changed");
        assert_eq!(after.head, before.head, "source HEAD changed");
        assert_eq!(after.index, before.index, "source index changed");
        assert_eq!(after.worktree, before.worktree, "source worktree changed");
    }

    struct RepoState {
        object_store: [u8; 32],
        refs: Vec<u8>,
        head: Vec<u8>,
        index: Vec<(PathBuf, Vec<u8>)>,
        worktree: BTreeMap<PathBuf, (u32, Vec<u8>)>,
    }

    fn repo_state(repo: &Path) -> RepoState {
        let object_dir = git_path(repo, ["rev-parse", "--git-path", "objects"]);
        let index_path = git_path(repo, ["rev-parse", "--git-path", "index"]);
        let mut index = vec![(PathBuf::from(&index_path), fs::read(&index_path).unwrap())];
        let shared_index = git_output(repo, ["rev-parse", "--shared-index-path"]);
        if !shared_index.is_empty() {
            let shared_path = PathBuf::from(shared_index);
            let shared_path = if shared_path.is_absolute() {
                shared_path
            } else {
                repo.join(shared_path)
            };
            index.push((shared_path.clone(), fs::read(shared_path).unwrap()));
        }
        RepoState {
            object_store: digest_tree(Path::new(&object_dir)),
            refs: git_raw_output(
                repo,
                ["for-each-ref", "--format=%(refname)%00%(objectname)"],
            ),
            head: fs::read(repo.join(".git/HEAD")).unwrap(),
            index,
            worktree: worktree_files(repo),
        }
    }

    fn digest_tree(root: &Path) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for entry in WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_dir() {
                continue;
            }
            let relative = entry.path().strip_prefix(root).unwrap();
            let path = relative.to_string_lossy();
            hasher.update((path.len() as u64).to_be_bytes());
            hasher.update(path.as_bytes());
            if entry.file_type().is_symlink() {
                let target = fs::read_link(entry.path()).unwrap();
                let target = target.to_string_lossy();
                hasher.update(target.as_bytes());
            } else {
                hasher.update(fs::read(entry.path()).unwrap());
            }
        }
        hasher.finalize().into()
    }

    fn worktree_files(repo: &Path) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
        let mut files = BTreeMap::new();
        for entry in WalkDir::new(repo)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != OsStr::new(".git"))
            .filter_map(Result::ok)
        {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }
            let relative = entry.path().strip_prefix(repo).unwrap().to_path_buf();
            let mode = entry.metadata().unwrap().permissions().mode();
            let bytes = if entry.file_type().is_symlink() {
                fs::read_link(entry.path())
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec()
            } else {
                fs::read(entry.path()).unwrap()
            };
            files.insert(relative, (mode, bytes));
        }
        files
    }

    fn git(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
        let mut command = Command::new("git");
        command.arg("-C").arg(repo).args(args);
        command.env("GIT_OPTIONAL_LOCKS", "0");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_with_index(
        repo: &Path,
        index: &Path,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> String {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_INDEX_FILE", index)
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git_output(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> String {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git_raw_output(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Vec<u8> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn git_path(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> String {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
