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
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const POINTER_LIMIT: u64 = 1024;
const OBJECT_BATCH_SIZE: usize = 512;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
pub struct LfsObject {
    pub oid: String,
    pub size: u64,
}

/// Fail closed when Git LFS would send uploads to an endpoint other than the
/// selected GitHub repository. Call immediately before `git lfs push`.
pub fn validate_lfs_push_endpoint(repo: &Path, remote: &str) -> Result<(), String> {
    validate_lfs_endpoint(repo, remote, true)
}

/// Fail closed when Git LFS would download objects from another GitHub repo.
fn validate_lfs_fetch_endpoint(repo: &Path, remote: &str) -> Result<(), String> {
    validate_lfs_endpoint(repo, remote, false)
}

fn validate_lfs_endpoint(repo: &Path, remote: &str, pushing: bool) -> Result<(), String> {
    if remote.is_empty() || remote.starts_with('-') || remote.bytes().any(|b| b.is_ascii_control())
    {
        return Err("invalid LFS endpoint remote".into());
    }
    let mut target_command = git();
    target_command
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url"]);
    if pushing {
        target_command.arg("--push");
    }
    target_command.arg(remote).stderr(Stdio::null());
    let target_output = target_command
        .output()
        .map_err(|_| "cannot inspect selected Git remote URL".to_owned())?;
    if !target_output.status.success() {
        return Err("cannot inspect selected Git remote URL".into());
    }
    let target = std::str::from_utf8(&target_output.stdout)
        .map_err(|_| "selected Git remote URL is not UTF-8")?
        .trim();
    // Local fixture remotes and non-GitHub deployments retain their existing
    // behavior. The recovery workflow's target is GitHub; enforce exact repo
    // ownership whenever that is the selected host.
    let Some(expected_repo) = github_repo_path(target) else {
        return Ok(());
    };

    let mut env_command = git();
    env_command
        .arg("-C")
        .arg(repo)
        .args(["lfs", "env"])
        .stderr(Stdio::null());
    let env_output = env_command
        .output()
        .map_err(|_| "cannot inspect effective Git LFS endpoint".to_owned())?;
    if !env_output.status.success() {
        return Err("cannot inspect effective Git LFS endpoint".into());
    }
    let env_text = std::str::from_utf8(&env_output.stdout)
        .map_err(|_| "effective Git LFS endpoint is not UTF-8")?;
    let remote_endpoint_prefix = format!("Endpoint ({remote})=");
    let endpoint_values = env_text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("Endpoint=")
                .or_else(|| line.strip_prefix(&remote_endpoint_prefix))
        })
        .map(|value| value.split_once(" (auth=").map_or(value, |(url, _)| url));
    let mut found_endpoint = false;
    for endpoint in endpoint_values {
        found_endpoint = true;
        if github_repo_path(endpoint).as_deref() != Some(expected_repo.as_str()) {
            return Err("Git LFS endpoint does not match the selected GitHub repository".into());
        }
    }
    if !found_endpoint {
        return Err("Git LFS did not report an effective endpoint".into());
    }

    // `git lfs env` reports the download endpoint. Push URLs have separate
    // configuration keys, so inspect all effective config scopes and the
    // checked-out .lfsconfig for values applicable to the selected remote.
    let configured = lfs_endpoint_config_values(repo, remote, pushing)?;
    for endpoint in configured {
        if github_repo_path(&endpoint).as_deref() != Some(expected_repo.as_str()) {
            return Err(
                "configured Git LFS push endpoint does not match the selected GitHub repository"
                    .into(),
            );
        }
    }
    Ok(())
}

fn github_repo_path(url: &str) -> Option<String> {
    let (host, path) = if let Some(rest) = url.strip_prefix("git@github.com:") {
        ("github.com", rest)
    } else if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        (host, path)
    } else {
        return None;
    };
    if !host.eq_ignore_ascii_case("github.com") || url.contains(['?', '#']) {
        return None;
    }
    let mut pieces = path.trim_matches('/').split('/');
    let owner = pieces.next()?;
    let mut name = pieces.next()?.to_owned();
    if let Some(stem) = name.strip_suffix(".git") {
        name = stem.to_owned();
    }
    let rest = pieces.collect::<Vec<_>>();
    if rest.is_empty() || rest == ["info", "lfs"] {
        Some(format!(
            "{}/{}",
            owner.to_ascii_lowercase(),
            name.to_ascii_lowercase()
        ))
    } else {
        None
    }
}

fn lfs_endpoint_config_values(
    repo: &Path,
    remote: &str,
    pushing: bool,
) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    let mut command = git();
    command
        .arg("-C")
        .arg(repo)
        .args(["config", "--null", "--list"])
        .stderr(Stdio::null());
    let output = command
        .output()
        .map_err(|_| "cannot inspect Git LFS endpoint configuration".to_owned())?;
    if !output.status.success() {
        return Err("cannot inspect Git LFS endpoint configuration".into());
    }
    values.extend(parse_lfs_config_values(&output.stdout, remote, pushing)?);

    let lfsconfig = repo.join(".lfsconfig");
    if lfsconfig.is_file() {
        let mut command = git();
        command
            .arg("-C")
            .arg(repo)
            .args(["config", "--null", "--list", "--file"])
            .arg(lfsconfig)
            .stderr(Stdio::null());
        let output = command
            .output()
            .map_err(|_| "cannot inspect repository LFS endpoint configuration".to_owned())?;
        if !output.status.success() {
            return Err("cannot inspect repository LFS endpoint configuration".into());
        }
        values.extend(parse_lfs_config_values(&output.stdout, remote, pushing)?);
    }
    Ok(values)
}

fn parse_lfs_config_values(
    config: &[u8],
    remote: &str,
    pushing: bool,
) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    for record in config
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let (key, value) = record
            .split_once(|byte| *byte == b'\n')
            .ok_or("malformed Git LFS endpoint configuration")?;
        let key = std::str::from_utf8(key)
            .map_err(|_| "Git LFS endpoint config key is not UTF-8")?
            .to_ascii_lowercase();
        let value =
            std::str::from_utf8(value).map_err(|_| "Git LFS endpoint config value is not UTF-8")?;
        let selected_key = key == "lfs.url"
            || (pushing && key == "lfs.pushurl")
            || key == format!("remote.{}.lfsurl", remote.to_ascii_lowercase())
            || (pushing && key == format!("remote.{}.lfspushurl", remote.to_ascii_lowercase()));
        if selected_key {
            values.push(value.to_owned());
        }
    }
    Ok(values)
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
    reject_matching_url_rewrite(remote)?;

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

    validate_lfs_fetch_endpoint(&repo, "origin")?;

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

/// Verify fresh remote LFS payloads and require the exact saved pointer set.
pub fn verify_remote_lfs_refs_match(
    remote: &str,
    refs: &[String],
    expected: &[LfsObject],
) -> Result<(), String> {
    let actual = verify_remote_lfs_refs(remote, refs)?;
    let expected = expected.iter().cloned().collect::<BTreeSet<_>>();
    let actual = actual.into_iter().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "remote LFS pointer set differs from saved inventory (expected {}, fetched {})",
            expected.len(),
            actual.len()
        ));
    }
    Ok(())
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
    for commit in commits {
        if (commit.len() != 40 && commit.len() != 64)
            || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid saved commit ID for LFS inventory".into());
        }
    }
    collect_pointer_objects_from_roots(repo, commits, "enumerate saved commit LFS pointers")
}

/// Check that every requested local object exists and matches its pointer
/// before a caller starts a batch upload. When Git LFS storage is private to
/// this Git store, also reject valid local payloads not reachable from the
/// supplied recovery roots; otherwise deleting the source would lose them.
pub fn validate_local_lfs_payloads(repo: &Path, objects: &[LfsObject]) -> Result<(), String> {
    let media_dir = local_media_dir(repo)?;
    let local_objects = inventory_private_local_lfs_objects(repo, &media_dir)?;
    let mut expected = BTreeMap::new();
    for object in objects {
        validate_object(object)?;
        if expected
            .insert(object.oid.clone(), object.size)
            .is_some_and(|old| old != object.size)
        {
            return Err(format!(
                "conflicting local LFS pointer sizes for {}",
                object.oid
            ));
        }
        if let Some(local_objects) = &local_objects {
            match local_objects.get(&object.oid) {
                Some(local) if local.size == object.size => {}
                Some(_) => {
                    return Err(format!("Git LFS payload size mismatch: {}", object.oid));
                }
                None => verify_payload(&media_dir, object)?,
            }
        } else {
            verify_payload(&media_dir, object)?;
        }
    }
    if let Some(local_objects) = local_objects {
        if let Some(orphan) = local_objects
            .keys()
            .find(|oid| !expected.contains_key(*oid))
        {
            return Err(format!(
                "unreferenced local Git LFS payload prevents safe cleanup: {orphan}"
            ));
        }
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
    collect_pointer_objects_from_roots(repo, refs, "enumerate LFS verifier history")
}

fn collect_pointer_objects_from_roots(
    repo: &Path,
    roots: &[String],
    operation: &str,
) -> Result<Vec<LfsObject>, String> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }

    ensure_complete_history_view(repo)?;

    // Walk the object closure once for all roots. Re-running ls-tree for each
    // commit repeats work on every shared ancestor and starts one Git process
    // per historical commit. rev-list emits the unique closure directly.
    let mut command = git();
    command
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--objects", "--no-object-names"])
        .args(roots)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| format!("{operation}: could not start Git"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{operation}: Git output unavailable"))?;
    let mut reader = BufReader::new(stdout);
    let mut object_ids = Vec::with_capacity(OBJECT_BATCH_SIZE);
    let mut pointers = BTreeMap::<String, u64>::new();
    let mut saw_object = false;
    let mut scan_error = None;
    loop {
        let mut line = Vec::new();
        let read = match reader.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(_) => {
                scan_error = Some(format!("{operation}: could not read Git object IDs"));
                break;
            }
        };
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let oid = match std::str::from_utf8(&line) {
            Ok(oid) => oid,
            Err(_) => {
                scan_error = Some("non-UTF8 object ID in LFS history".into());
                break;
            }
        };
        if (oid.len() != 40 && oid.len() != 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            scan_error = Some("malformed object ID in LFS history".into());
            break;
        }
        saw_object = true;
        object_ids.push(oid.to_owned());
        if object_ids.len() == OBJECT_BATCH_SIZE {
            if let Err(error) = scan_object_batch(repo, &object_ids, &mut pointers) {
                scan_error = Some(error);
                break;
            }
            object_ids.clear();
        }
    }
    if scan_error.is_some() {
        let _ = child.kill();
    }
    drop(reader);
    let status = child
        .wait()
        .map_err(|_| format!("{operation}: could not wait for Git"))?;
    if let Some(error) = scan_error {
        return Err(error);
    }
    if !status.success() {
        return Err(format!("{operation} failed ({status})"));
    }
    if !saw_object {
        return Err("LFS history walk returned no objects for nonempty roots".into());
    }
    if !object_ids.is_empty() {
        scan_object_batch(repo, &object_ids, &mut pointers)?;
    }
    Ok(pointers
        .into_iter()
        .map(|(oid, size)| LfsObject { oid, size })
        .collect())
}

fn scan_object_batch(
    repo: &Path,
    object_ids: &[String],
    pointers: &mut BTreeMap<String, u64>,
) -> Result<(), String> {
    let metadata = run_cat_file_batch(
        repo,
        "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        &object_ids,
        "inspect LFS history objects",
    )?;
    let mut candidate_ids = Vec::new();
    let mut candidate_sizes = Vec::new();
    let rows = metadata
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if rows.len() != object_ids.len() {
        return Err("incomplete batched LFS object inventory".into());
    }
    for (expected, row) in object_ids.iter().zip(rows) {
        let fields = row.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.len() != 3 || fields[0] != expected.as_bytes() {
            return Err("malformed batched LFS object metadata".into());
        }
        if fields[1] != b"blob" {
            continue;
        }
        let size = std::str::from_utf8(fields[2])
            .map_err(|_| "Git blob size is not UTF-8")?
            .parse::<u64>()
            .map_err(|_| "invalid Git blob size")?;
        if size > 0 && size <= POINTER_LIMIT {
            candidate_ids.push(expected.clone());
            candidate_sizes.push(size);
        }
    }

    let contents = if candidate_ids.is_empty() {
        Vec::new()
    } else {
        run_cat_file_batch(
            repo,
            "--batch",
            &candidate_ids,
            "read possible LFS pointers",
        )?
    };
    let mut cursor = 0;
    for (oid, size) in candidate_ids.iter().zip(candidate_sizes) {
        let header_end = contents[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| cursor + offset)
            .ok_or("malformed batched LFS blob response")?;
        let header = contents[cursor..header_end]
            .split(|byte| *byte == b' ')
            .collect::<Vec<_>>();
        if header.len() != 3
            || header[0] != oid.as_bytes()
            || header[1] != b"blob"
            || header[2] != size.to_string().as_bytes()
        {
            return Err("malformed batched LFS blob metadata".into());
        }
        cursor = header_end + 1;
        let end = cursor
            .checked_add(size as usize)
            .ok_or("Git blob size overflow")?;
        if end >= contents.len() || contents[end] != b'\n' {
            return Err("truncated batched LFS blob response".into());
        }
        if let Some(object) = parse_pointer(&contents[cursor..end])? {
            if pointers
                .insert(object.oid.clone(), object.size)
                .is_some_and(|old| old != object.size)
            {
                return Err(format!("conflicting LFS pointer sizes for {}", object.oid));
            }
        }
        cursor = end + 1;
    }
    if cursor != contents.len() {
        return Err("unexpected data in batched LFS blob response".into());
    }
    Ok(())
}

fn ensure_complete_history_view(repo: &Path) -> Result<(), String> {
    let mut command = git();
    command
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--is-shallow-repository"]);
    let output = git_output(command, "inspect Git history completeness")?;
    match output.as_slice() {
        b"true\n" | b"true" => {
            return Err("LFS inventory blocked: shallow Git history is incomplete".into());
        }
        b"false\n" | b"false" => {}
        _ => return Err("LFS inventory blocked: invalid shallow-history status".into()),
    }

    let mut command = git();
    command
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-path", "info/grafts"]);
    let output = git_output(command, "inspect Git graft configuration")?;
    let path = std::str::from_utf8(&output)
        .map_err(|_| "Git graft path is not UTF-8")?
        .trim();
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    };
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > 0 => {
            Err("LFS inventory blocked: Git grafts alter commit history".into())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("LFS inventory blocked: cannot inspect Git graft configuration".into()),
    }
}

fn reject_matching_url_rewrite(remote: &str) -> Result<(), String> {
    for scope in ["--system", "--global"] {
        let mut command = git();
        command.args([
            "config",
            "--null",
            scope,
            "--get-regexp",
            r"^url\..*\.insteadof$",
        ]);
        let output = command
            .output()
            .map_err(|_| "cannot inspect Git URL rewrite configuration".to_owned())?;
        if !output.status.success() {
            if output.status.code() == Some(1) {
                continue;
            }
            return Err("cannot inspect Git URL rewrite configuration".into());
        }
        let prefixes = url_rewrite_prefixes(&output.stdout)?;
        if url_rewrite_matches(remote, &prefixes) {
            return Err(
                "LFS verification blocked: a configured Git URL rewrite matches the selected remote".into(),
            );
        }
    }
    Ok(())
}

fn url_rewrite_prefixes(config: &[u8]) -> Result<Vec<&str>, String> {
    config
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let newline = record
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or("malformed Git URL rewrite configuration")?;
            let (_, value) = record.split_at(newline + 1);
            std::str::from_utf8(value).map_err(|_| "Git URL rewrite prefix is not UTF-8".into())
        })
        .collect()
}

fn url_rewrite_matches(remote: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| remote.starts_with(prefix))
}

fn run_cat_file_batch(
    repo: &Path,
    mode: &str,
    object_ids: &[String],
    operation: &str,
) -> Result<Vec<u8>, String> {
    let mut command = git();
    command
        .arg("-C")
        .arg(repo)
        .arg("cat-file")
        .arg(mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| format!("{operation}: could not start Git"))?;
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{operation}: Git input unavailable"));
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{operation}: Git output unavailable"));
        }
    };
    let ids = object_ids.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for oid in ids {
            writeln!(stdin, "{oid}")?;
        }
        Ok(())
    });
    let mut output = Vec::new();
    let read_result = stdout.read_to_end(&mut output);
    drop(stdout);
    let write_result = writer.join();
    let status = child
        .wait()
        .map_err(|_| format!("{operation}: could not wait for Git"))?;
    read_result.map_err(|_| format!("{operation}: could not read Git output"))?;
    write_result
        .map_err(|_| format!("{operation}: Git input worker failed"))?
        .map_err(|_| format!("{operation}: could not send object IDs to Git"))?;
    if !status.success() {
        return Err(format!("{operation} failed ({status})"));
    }
    Ok(output)
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

fn inventory_private_local_lfs_objects(
    repo: &Path,
    media_dir: &Path,
) -> Result<Option<BTreeMap<String, LfsObject>>, String> {
    let mut command = git();
    command
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"]);
    let output = git_output(command, "locate Git object store for LFS inventory")?;
    let common = std::str::from_utf8(&output)
        .map_err(|_| "Git common directory path is not UTF-8")?
        .trim();
    if common.is_empty() {
        return Err("Git common directory path is empty".into());
    }
    let common = Path::new(common);
    let common = if common.is_absolute() {
        common.to_path_buf()
    } else {
        repo.join(common)
    };
    let common = fs::canonicalize(common)
        .map_err(|_| "cannot resolve Git common directory for LFS inventory")?;
    let media = match fs::canonicalize(media_dir) {
        Ok(path) if path.starts_with(&common) => path,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(BTreeMap::new()));
        }
        Err(_) => return Err("cannot resolve local Git LFS storage".into()),
    };

    let mut objects = BTreeMap::new();
    for shard in read_directory(&media)? {
        let shard_type = shard
            .file_type()
            .map_err(|_| "cannot inspect local Git LFS object directory")?;
        let shard_file_name = shard.file_name();
        let shard_name = os_name(&shard_file_name)?;
        if !shard_type.is_dir() || !is_lower_hex(shard_name, 2) {
            return Err("unsupported entry in private Git LFS object storage".into());
        }
        for subshard in read_directory(&shard.path())? {
            let subshard_type = subshard
                .file_type()
                .map_err(|_| "cannot inspect local Git LFS object directory")?;
            let subshard_file_name = subshard.file_name();
            let subshard_name = os_name(&subshard_file_name)?;
            if !subshard_type.is_dir() || !is_lower_hex(subshard_name, 2) {
                return Err("unsupported entry in private Git LFS object storage".into());
            }
            for entry in read_directory(&subshard.path())? {
                let file_type = entry
                    .file_type()
                    .map_err(|_| "cannot inspect local Git LFS payload")?;
                let entry_file_name = entry.file_name();
                let oid = os_name(&entry_file_name)?;
                if !file_type.is_file()
                    || !is_lower_hex(oid, 64)
                    || !oid.starts_with(shard_name)
                    || !oid[2..].starts_with(subshard_name)
                {
                    return Err("unsupported entry in private Git LFS object storage".into());
                }
                let metadata = entry
                    .metadata()
                    .map_err(|_| "cannot inspect local Git LFS payload")?;
                let object = LfsObject {
                    oid: oid.to_owned(),
                    size: metadata.len(),
                };
                verify_payload(&media, &object)?;
                if objects.insert(oid.to_owned(), object).is_some() {
                    return Err("duplicate local Git LFS payload path".into());
                }
            }
        }
    }
    Ok(Some(objects))
}

fn read_directory(path: &Path) -> Result<Vec<fs::DirEntry>, String> {
    fs::read_dir(path)
        .map_err(|_| "cannot read private Git LFS object storage".to_owned())?
        .map(|entry| entry.map_err(|_| "cannot read private Git LFS object storage".to_owned()))
        .collect()
}

fn os_name(name: &std::ffi::OsStr) -> Result<&str, String> {
    name.to_str()
        .ok_or_else(|| "non-UTF8 entry in private Git LFS object storage".into())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SHALLOW_FILE",
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
    // Scan the object graph as stored, not the compatibility view substituted
    // by refs/replace. Replacement refs are preserved separately by the caller.
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
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

#[cfg(test)]
mod tests {
    use super::{
        url_rewrite_matches, url_rewrite_prefixes, validate_lfs_fetch_endpoint,
        validate_lfs_push_endpoint,
    };
    use std::{fs, path::Path, process::Command};

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git command failed");
    }

    fn endpoint_fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/project.git",
            ],
        );
        temp
    }

    #[test]
    fn matching_git_url_rewrite_blocks_selected_remote() {
        let config = b"url.https://mirror.invalid/.insteadof\nhttps://github.com/\0";
        let prefixes = url_rewrite_prefixes(config).unwrap();
        assert!(url_rewrite_matches(
            "https://github.com/acme/project.git",
            &prefixes
        ));
        assert!(!url_rewrite_matches(
            "https://other.invalid/acme/project.git",
            &prefixes
        ));
    }

    #[test]
    fn selected_github_repository_accepts_derived_lfs_endpoints() {
        let temp = endpoint_fixture();
        let repo = temp.path().join("repo");
        validate_lfs_fetch_endpoint(&repo, "origin").unwrap();
        validate_lfs_push_endpoint(&repo, "origin").unwrap();
    }

    #[test]
    fn lfs_push_redirect_is_blocked_without_printing_credentials() {
        let temp = endpoint_fixture();
        let repo = temp.path().join("repo");
        git(
            &repo,
            &[
                "config",
                "lfs.pushurl",
                "https://user:secret-token@attacker.invalid/acme/project/info/lfs",
            ],
        );
        let error = validate_lfs_push_endpoint(&repo, "origin").unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert!(!error.contains("secret-token"), "{error}");
    }

    #[test]
    fn committed_lfsconfig_download_redirect_is_blocked() {
        let temp = endpoint_fixture();
        let repo = temp.path().join("repo");
        fs::write(
            repo.join(".lfsconfig"),
            "[lfs]\n\turl = https://attacker.invalid/acme/project/info/lfs\n",
        )
        .unwrap();
        let error = validate_lfs_fetch_endpoint(&repo, "origin").unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn matching_lfs_pushurl_is_allowed() {
        let temp = endpoint_fixture();
        let repo = temp.path().join("repo");
        git(
            &repo,
            &[
                "config",
                "remote.origin.lfspushurl",
                "https://github.com/acme/project.git/info/lfs",
            ],
        );
        validate_lfs_push_endpoint(&repo, "origin").unwrap();
    }
}
