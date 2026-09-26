//! Isolated verification of Git LFS objects referenced by remote Git refs.
//!
//! Git commits store LFS pointers, not payload bytes. This module fetches the
//! refs and payloads into a disposable repository with its own LFS storage,
//! then checks every downloaded payload against the pointer's SHA-256 and size.

use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const POINTER_LIMIT: u64 = 1024;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
pub struct LfsObject {
    pub oid: String,
    pub size: u64,
}

/// Fetch `refs` and all LFS payloads they reference from `remote`, using a
/// temporary Git repository and fresh LFS media directory.
pub fn verify_remote_lfs_refs(remote: &str, refs: &[String]) -> Result<Vec<LfsObject>, String> {
    if remote.is_empty() || remote.starts_with('-') || remote.bytes().any(|b| b.is_ascii_control())
    {
        return Err("invalid LFS verification remote".into());
    }
    let refs = normalize_refs(refs)?;
    if refs.is_empty() {
        return Ok(Vec::new());
    }

    let temp = tempfile::tempdir().map_err(|e| format!("create isolated LFS verifier: {e}"))?;
    let repo = temp.path().join("repo");
    let media = temp.path().join("lfs-media");
    fs::create_dir_all(&media).map_err(|e| format!("create isolated LFS storage: {e}"))?;
    let repo_s = repo
        .to_str()
        .ok_or("non-UTF8 isolated LFS repository path")?;

    let mut command = git();
    command.args(["init", "--quiet", repo_s]);
    checked(command, "initialize isolated LFS verifier")?;
    let mut command = git();
    command.args([
        "-C",
        repo_s,
        "config",
        "lfs.storage",
        media.to_str().ok_or("non-UTF8 isolated LFS storage path")?,
    ]);
    checked(command, "configure isolated LFS storage")?;
    let mut command = git();
    command.args(["-C", repo_s, "remote", "add", "origin", remote]);
    checked(command, "configure isolated LFS verifier remote")?;

    let mut fetch = vec![
        OsString::from("-C"),
        repo.as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from("fetch.fsckObjects=true"),
        OsString::from("fetch"),
        OsString::from("--no-tags"),
        OsString::from("--no-recurse-submodules"),
        OsString::from("origin"),
    ];
    let mut verify_refs = Vec::with_capacity(refs.len());
    for (index, reference) in refs.iter().enumerate() {
        let destination = format!("refs/verify/lfs/{index}");
        fetch.push(format!("{reference}:{destination}").into());
        verify_refs.push(destination);
    }
    let mut command = git();
    command.args(fetch);
    checked(command, "fetch refs into isolated LFS verifier")?;

    let mut checkout = git();
    checkout
        .arg("-C")
        .arg(&repo)
        .args(["checkout", "--quiet", "--detach"])
        .arg(&verify_refs[0]);
    checked(checkout, "read isolated LFS endpoint configuration")?;

    let objects = collect_pointer_objects(&repo, &verify_refs)?;
    if objects.is_empty() {
        return Ok(objects);
    }

    let mut fetch_lfs = git();
    fetch_lfs
        .arg("-C")
        .arg(&repo)
        .args(["lfs", "fetch", "--all", "origin"])
        .args(&verify_refs);
    checked(fetch_lfs, "fetch LFS payloads into isolated verifier")?;

    let media_dir = local_media_dir(&repo)?;
    for object in &objects {
        verify_payload(&media_dir, object)?;
    }
    Ok(objects)
}

/// Read all LFS pointers reachable from the supplied refs in a local repo.
pub fn inventory_local_lfs_objects(repo: &Path, refs: &[String]) -> Result<Vec<LfsObject>, String> {
    let refs = normalize_refs(refs)?;
    collect_pointer_objects(repo, &refs)
}

/// Inventory LFS pointers reachable from saved commit IDs, including
/// snapshot commits that have no local branch or ref.
pub fn inventory_local_lfs_commits(
    repo: &Path,
    commits: &[String],
) -> Result<Vec<LfsObject>, String> {
    let mut seen = BTreeSet::new();
    let mut pointers = BTreeMap::<String, u64>::new();
    for commit in commits {
        if (commit.len() != 40 && commit.len() != 64)
            || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid saved commit ID for LFS inventory".into());
        }
        if !seen.insert(commit) {
            continue;
        }
        let mut command = git();
        command
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "-r", "-z", "--full-tree", commit]);
        let output = git_output(command, "enumerate saved commit LFS pointers")?;
        for entry in output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let separator = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or("malformed saved tree entry")?;
            let mut fields = entry[..separator].split(|byte| *byte == b' ');
            let _mode = fields.next().ok_or("malformed saved tree mode")?;
            let kind = fields.next().ok_or("malformed saved tree object type")?;
            let oid = fields.next().ok_or("malformed saved tree object ID")?;
            if kind != b"blob" {
                continue;
            }
            let oid = std::str::from_utf8(oid).map_err(|_| "non-UTF8 saved blob ID")?;
            let size = blob_size(repo, oid)?;
            if size == 0 || size > POINTER_LIMIT {
                continue;
            }
            let mut command = git();
            command.arg("-C").arg(repo).args(["cat-file", "blob", oid]);
            let content = git_output(command, "read saved possible LFS pointer")?;
            if let Some(object) = parse_pointer(&content)? {
                if pointers
                    .insert(object.oid.clone(), object.size)
                    .is_some_and(|old| old != object.size)
                {
                    return Err(format!("conflicting LFS pointer sizes for {}", object.oid));
                }
            }
        }
    }
    Ok(pointers
        .into_iter()
        .map(|(oid, size)| LfsObject { oid, size })
        .collect())
}

/// Check that every requested local object exists and matches its pointer
/// before a caller starts a batch upload.
pub fn validate_local_lfs_payloads(repo: &Path, objects: &[LfsObject]) -> Result<(), String> {
    let media_dir = local_media_dir(repo)?;
    for object in objects {
        validate_object(object)?;
        verify_payload(&media_dir, object)?;
    }
    Ok(())
}

fn normalize_refs(refs: &[String]) -> Result<Vec<String>, String> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for reference in refs {
        if !reference.starts_with("refs/") || reference.bytes().any(|b| b.is_ascii_control()) {
            return Err(format!("invalid LFS verification ref: {reference}"));
        }
        let mut command = git();
        command.args(["check-ref-format", reference]);
        checked(command, "validate LFS verification ref")?;
        if seen.insert(reference.clone()) {
            normalized.push(reference.clone());
        }
    }
    Ok(normalized)
}

fn collect_pointer_objects(repo: &Path, refs: &[String]) -> Result<Vec<LfsObject>, String> {
    let mut pointers = BTreeMap::<String, u64>::new();
    for reference in refs {
        let mut command = git();
        command
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "-r", "-z", "--full-tree", reference]);
        let output = git_output(command, "enumerate LFS verifier tree")?;
        for entry in output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let separator = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or("malformed tree entry in LFS verifier")?;
            let metadata = &entry[..separator];
            let mut fields = metadata.split(|byte| *byte == b' ');
            let _mode = fields.next().ok_or("malformed tree mode")?;
            let kind = fields.next().ok_or("malformed tree object type")?;
            let oid = fields.next().ok_or("malformed tree object ID")?;
            if kind != b"blob" {
                continue;
            }
            let oid = std::str::from_utf8(oid).map_err(|_| "non-UTF8 Git blob ID")?;
            let size = blob_size(repo, oid)?;
            if size == 0 || size > POINTER_LIMIT {
                continue;
            }
            let mut command = git();
            command.arg("-C").arg(repo).args(["cat-file", "blob", oid]);
            let content = git_output(command, "read possible LFS pointer")?;
            if let Some(object) = parse_pointer(&content)? {
                if pointers
                    .insert(object.oid.clone(), object.size)
                    .is_some_and(|old| old != object.size)
                {
                    return Err(format!("conflicting LFS pointer sizes for {}", object.oid));
                }
            }
        }
    }
    Ok(pointers
        .into_iter()
        .map(|(oid, size)| LfsObject { oid, size })
        .collect())
}

fn blob_size(repo: &Path, oid: &str) -> Result<u64, String> {
    let mut command = git();
    command.arg("-C").arg(repo).args(["cat-file", "-s", oid]);
    let output = git_output(command, "read Git blob size")?;
    String::from_utf8(output)
        .map_err(|_| "Git blob size is not UTF-8".to_owned())?
        .trim()
        .parse()
        .map_err(|_| "invalid Git blob size".to_owned())
}

fn parse_pointer(bytes: &[u8]) -> Result<Option<LfsObject>, String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let mut version = false;
    let mut oid = None;
    let mut size = None;
    for line in text.lines() {
        if line == "version https://git-lfs.github.com/spec/v1" {
            version = true;
        } else if let Some(value) = line.strip_prefix("oid sha256:") {
            if oid.replace(value.to_owned()).is_some() {
                return Err("duplicate OID in Git LFS pointer".into());
            }
        } else if let Some(value) = line.strip_prefix("size ") {
            if size
                .replace(
                    value
                        .parse::<u64>()
                        .map_err(|_| "invalid size in Git LFS pointer")?,
                )
                .is_some()
            {
                return Err("duplicate size in Git LFS pointer".into());
            }
        }
    }
    if !version {
        return Ok(None);
    }
    let object = LfsObject {
        oid: oid.ok_or("Git LFS pointer has no SHA-256 OID")?,
        size: size.ok_or("Git LFS pointer has no size")?,
    };
    validate_object(&object)?;
    Ok(Some(object))
}

fn validate_object(object: &LfsObject) -> Result<(), String> {
    if object.oid.len() != 64 || !object.oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid Git LFS object ID; expected 64 hexadecimal characters".into());
    }
    Ok(())
}

fn local_media_dir(repo: &Path) -> Result<PathBuf, String> {
    let mut command = git();
    command.arg("-C").arg(repo).args(["lfs", "env"]);
    let output = git_output(command, "read isolated LFS storage")?;
    let text = std::str::from_utf8(&output).map_err(|_| "Git LFS environment is not UTF-8")?;
    text.lines()
        .find_map(|line| line.strip_prefix("LocalMediaDir="))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "Git LFS did not report local object storage".into())
}

fn verify_payload(media: &Path, object: &LfsObject) -> Result<(), String> {
    validate_object(object)?;
    let path = media
        .join(&object.oid[..2])
        .join(&object.oid[2..4])
        .join(&object.oid);
    let mut file = fs::File::open(&path)
        .map_err(|_| format!("missing downloaded Git LFS payload: {}", object.oid))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| format!("cannot read Git LFS payload: {}", object.oid))?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or("Git LFS payload size overflow")?;
        hasher.update(&buffer[..read]);
    }
    let actual_oid = format!("{:x}", hasher.finalize());
    if actual_oid != object.oid {
        return Err(format!("Git LFS payload SHA-256 mismatch: {}", object.oid));
    }
    if size != object.size {
        return Err(format!("Git LFS payload size mismatch: {}", object.oid));
    }
    Ok(())
}

fn git() -> Command {
    isolated_command("git")
}

fn isolated_command(program: &str) -> Command {
    let mut command = Command::new(program);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_CONFIG_KEY_")
            || name.to_string_lossy().starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(name);
        }
    }
    command.env("GIT_NO_LAZY_FETCH", "1");
    command.env("GIT_LFS_SKIP_SMUDGE", "1");
    command
}

fn git_output(mut command: Command, operation: &str) -> Result<Vec<u8>, String> {
    let output = command.output().map_err(|e| format!("{operation}: {e}"))?;
    checked_output(&output, operation)?;
    Ok(output.stdout)
}

fn checked(mut command: Command, operation: &str) -> Result<(), String> {
    let output = command.output().map_err(|e| format!("{operation}: {e}"))?;
    checked_output(&output, operation)
}

fn checked_output(output: &Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{operation} failed ({})", output.status))
    }
}
