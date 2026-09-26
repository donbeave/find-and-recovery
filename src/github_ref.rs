//! Upload commits and atomically create recovery refs without updating existing refs.
//!
//! The batch push path uploads exact local commits and asks Git's receive-pack
//! protocol to create each destination only if absent.

use std::process::Command;

/// Whether the atomic push was acknowledged or recovered from an exact remote read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateOnlyPushOutcome {
    /// Git acknowledged the atomic push and the remote has every requested tip.
    Acknowledged,
    /// Git returned an error, but a fresh remote read proved every requested tip.
    ReconciledAfterError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PushTransport {
    Other,
    Https,
    GitHubSsh,
}

#[cfg(unix)]
const SAFE_GITHUB_SSH_COMMAND: &str = "ssh -F /dev/null -o BatchMode=yes -o CanonicalizeHostname=no -o PermitLocalCommand=no -o ProxyCommand=none -o ProxyJump=none -o ConnectTimeout=10";

/// Upload exact local commits and atomically create absent recovery refs.
/// Each empty force-with-lease is an expected-old-zero condition for its exact
/// destination. There is no GitHub API or ordinary-push fallback.
pub fn push_data_create_only_batch(
    remote: &str,
    repo: &std::path::Path,
    updates: &[(String, String)],
) -> Result<CreateOnlyPushOutcome, String> {
    if updates.is_empty() {
        return Ok(CreateOnlyPushOutcome::Acknowledged);
    }

    // Reject malformed input before any subprocess. In particular, never let
    // an option-like destination or malformed ref reach Git's argument parser.
    if remote.is_empty()
        || remote.starts_with('-')
        || remote.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err("invalid recovery push destination".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for (reference, oid) in updates {
        if !valid_recovery_push_ref(reference)
            || !seen.insert(reference)
            || (oid.len() != 40 && oid.len() != 64)
            || !oid.bytes().all(|byte| {
                byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
            })
            || oid.bytes().all(|byte| byte == b'0')
        {
            return Err(format!("invalid create-only recovery update: {reference}"));
        }
    }

    validate_repo_config_path(repo)?;
    // Reject custom SSH helpers before any validation or mutation command can
    // use them. The command environment is also sanitized below.
    ensure_safe_ssh_environment(repo)?;
    let transport = validate_push_remote(repo, remote)?;
    reject_configured_push_options(repo)?;

    let format = git_repo_text(repo, &["rev-parse", "--show-object-format"])?;
    let oid_len = match format.trim() {
        "sha1" => 40,
        "sha256" => 64,
        other => return Err(format!("unsupported Git object format: {other}")),
    };
    for (reference, oid) in updates {
        if oid.len() != oid_len {
            return Err(format!(
                "recovery OID length does not match {format}: {reference}"
            ));
        }
        git_repo_text(repo, &["check-ref-format", reference])?;
    }
    validate_commit_oids(repo, updates)?;

    let push_result = run_create_only_push(remote, repo, updates, transport);
    ensure_safe_ssh_environment(repo)?;
    reconcile_create_only_push(remote, repo, updates, push_result, transport)
}

fn ensure_safe_ssh_environment(repo: &std::path::Path) -> Result<(), String> {
    crate::remote_lfs::reject_ssh_overrides(repo)
        .map_err(|_| "recovery push blocked by custom SSH or proxy configuration".to_owned())
}

fn validate_repo_config_path(repo: &std::path::Path) -> Result<(), String> {
    let path = repo
        .to_str()
        .ok_or_else(|| "recovery repository path is not UTF-8".to_owned())?;
    if path.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("recovery repository path contains control characters".into());
    }
    Ok(())
}

fn valid_recovery_push_ref(reference: &str) -> bool {
    if !reference.starts_with("refs/heads/recovery/")
        || reference.contains("..")
        || reference.contains("@{")
        || reference
            .bytes()
            .any(|byte| byte.is_ascii_control() || b"~^:?*[\\".contains(&byte))
    {
        return false;
    }
    reference.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part != "@"
            && !part.starts_with('.')
            && !part.ends_with('.')
            && !part.ends_with(".lock")
    })
}

fn validate_push_remote(repo: &std::path::Path, remote: &str) -> Result<PushTransport, String> {
    let remotes = git_repo_text(repo, &["remote"])?;
    if remotes.lines().any(|name| name == remote) {
        reject_named_remote_command_overrides(repo, remote)?;
        let fetch_key = format!("remote.{remote}.url");
        let push_key = format!("remote.{remote}.pushurl");
        let fetch_urls = git_config_values(repo, &fetch_key)?;
        let push_urls = git_config_values(repo, &push_key)?;
        if fetch_urls.len() != 1 || push_urls.len() > 1 {
            return Err("selected Git remote must have one fetch and at most one push URL".into());
        }
        let push_url = push_urls.first().unwrap_or(&fetch_urls[0]);
        if push_url != &fetch_urls[0] {
            return Err("selected Git remote has different fetch and push URLs".into());
        }
        reject_url_rewrites(repo, &[&fetch_urls[0], push_url])?;
        return validate_remote_transport(repo, push_url);
    }

    if is_explicit_git_remote(repo, remote) {
        reject_url_rewrites(repo, &[remote])?;
        return validate_remote_transport(repo, remote);
    }
    Err("selected Git remote is not configured".into())
}

fn validate_remote_transport(
    repo: &std::path::Path,
    remote_url: &str,
) -> Result<PushTransport, String> {
    let transport = if let Some((scheme, _)) = remote_url.split_once("://") {
        if scheme.eq_ignore_ascii_case("https") {
            PushTransport::Https
        } else if scheme.eq_ignore_ascii_case("ssh") {
            if !is_canonical_github_ssh_url(remote_url) {
                return Err("SSH recovery pushes require a canonical GitHub SSH URL".into());
            }
            PushTransport::GitHubSsh
        } else if scheme.to_ascii_lowercase().contains("ssh") {
            return Err("SSH recovery pushes require a canonical GitHub SSH URL".into());
        } else {
            PushTransport::Other
        }
    } else if looks_like_scp_remote(remote_url) {
        if !is_canonical_github_ssh_url(remote_url) {
            return Err("SSH recovery pushes require a canonical GitHub SSH URL".into());
        }
        PushTransport::GitHubSsh
    } else {
        PushTransport::Other
    };

    if transport == PushTransport::Https {
        reject_local_credential_helpers(repo)?;
    }
    if transport == PushTransport::GitHubSsh && !cfg!(unix) {
        return Err("safe GitHub SSH recovery push is unavailable on this platform".into());
    }
    Ok(transport)
}

fn looks_like_scp_remote(remote_url: &str) -> bool {
    if remote_url.starts_with('/')
        || remote_url.starts_with("./")
        || remote_url.starts_with("../")
        || remote_url.starts_with("~/")
        || std::path::Path::new(remote_url).exists()
        || (remote_url.as_bytes().get(1) == Some(&b':')
            && remote_url
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic))
    {
        return false;
    }
    let Some((host, path)) = remote_url.split_once(':') else {
        return false;
    };
    !host.is_empty() && !host.contains('/') && !path.is_empty()
}

fn is_canonical_github_ssh_url(remote_url: &str) -> bool {
    let path = if let Some(path) = remote_url.strip_prefix("git@github.com:") {
        path
    } else if let Some(rest) = remote_url.strip_prefix("ssh://") {
        let Some((authority, path)) = rest.split_once('/') else {
            return false;
        };
        if authority != "git@github.com" && authority != "git@github.com:22" {
            return false;
        }
        path
    } else {
        return false;
    };
    valid_github_ssh_repo_path(path)
}

fn valid_github_ssh_repo_path(path: &str) -> bool {
    let path = path.strip_suffix(".git").unwrap_or(path);
    let pieces = path.split('/').collect::<Vec<_>>();
    pieces.len() == 2
        && pieces.iter().all(|piece| {
            !piece.is_empty()
                && *piece != "."
                && *piece != ".."
                && piece
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn reject_local_credential_helpers(repo: &std::path::Path) -> Result<(), String> {
    let output = git_repo_command(repo)
        .args([
            "config",
            "--show-scope",
            "--name-only",
            "--get-regexp",
            r"^credential\.(helper|.*\.helper)$",
        ])
        .output()
        .map_err(|_| "could not inspect repository credential helper configuration".to_owned())?;
    if output.status.success() && !output.stdout.is_empty() {
        let scopes = String::from_utf8(output.stdout)
            .map_err(|_| "could not inspect repository credential helper configuration")?;
        for line in scopes.lines() {
            let (scope, _key) = line
                .split_once('\t')
                .ok_or("could not inspect repository credential helper configuration")?;
            if matches!(scope, "local" | "worktree") {
                return Err("HTTPS recovery push blocked by repository credential helper".into());
            }
            if !matches!(scope, "system" | "global" | "command") {
                return Err("could not inspect repository credential helper configuration".into());
            }
        }
    }
    if output.status.code() == Some(1) {
        return Ok(());
    }
    if !output.status.success() {
        return Err("could not inspect repository credential helper configuration".into());
    }
    Ok(())
}

fn reject_configured_push_options(repo: &std::path::Path) -> Result<(), String> {
    let output = git_repo_command(repo)
        .args([
            "config",
            "--show-scope",
            "--name-only",
            "--get-regexp",
            r"^push\.pushoption$",
        ])
        .output()
        .map_err(|_| "could not inspect Git push option configuration".to_owned())?;
    if output.status.success() && !output.stdout.is_empty() {
        return Err("recovery push blocked by configured Git push options".into());
    }
    if output.status.code() == Some(1) {
        return Ok(());
    }
    if !output.status.success() {
        return Err("could not inspect Git push option configuration".into());
    }
    Ok(())
}

fn reject_named_remote_command_overrides(
    repo: &std::path::Path,
    remote: &str,
) -> Result<(), String> {
    for command in ["uploadpack", "receivepack", "vcs"] {
        let key = format!("remote.{remote}.{command}");
        if !git_config_values(repo, &key)?.is_empty() {
            return Err("selected Git remote has a custom Git transport command".into());
        }
    }
    Ok(())
}

fn is_explicit_git_remote(repo: &std::path::Path, remote: &str) -> bool {
    let path = std::path::Path::new(remote);
    remote.contains("://")
        || remote.starts_with("git@")
        || (remote.contains('@') && remote.contains(':'))
        || path.is_absolute()
        || remote.starts_with("./")
        || remote.starts_with("../")
        || remote.starts_with("~/")
        || path.exists()
        || repo.join(path).exists()
}

fn reject_url_rewrites(repo: &std::path::Path, targets: &[&str]) -> Result<(), String> {
    let output = git_repo_command(repo)
        .args([
            "config",
            "--null",
            "--get-regexp",
            r"^url\..*\.(insteadof|pushinsteadof)$",
        ])
        .output()
        .map_err(|error| format!("start Git URL rewrite inspection: {error}"))?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(());
        }
        return Err("could not inspect Git URL rewrite configuration".into());
    }
    for entry in output.stdout.split(|byte| *byte == 0) {
        let Some(split) = entry.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let prefix = String::from_utf8_lossy(&entry[split + 1..]);
        if !prefix.is_empty()
            && targets
                .iter()
                .any(|target| target.starts_with(prefix.as_ref()))
        {
            return Err("selected Git remote is affected by a URL rewrite".into());
        }
    }
    Ok(())
}

fn git_config_values(repo: &std::path::Path, key: &str) -> Result<Vec<String>, String> {
    let output = git_repo_command(repo)
        .args(["config", "--null", "--get-all", key])
        .output()
        .map_err(|error| format!("start Git remote configuration lookup: {error}"))?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        return Err("could not inspect selected Git remote configuration".into());
    }
    let values = String::from_utf8(output.stdout)
        .map_err(|_| "selected Git remote configuration is not UTF-8".to_owned())?;
    Ok(values
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

fn validate_commit_oids(
    repo: &std::path::Path,
    updates: &[(String, String)],
) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = git_repo_command(repo)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start batched commit validation: {error}"))?;
    let input = updates
        .iter()
        .map(|(_, oid)| oid.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let write_result = child
        .stdin
        .take()
        .ok_or("batched commit validation stdin unavailable")?
        .write_all(input.as_bytes());
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("write batched commit validation: {error}"));
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for batched commit validation: {error}"))?;
    if !output.status.success() {
        return Err("batched commit validation failed".into());
    }
    let rows = String::from_utf8(output.stdout)
        .map_err(|_| "batched commit validation returned invalid UTF-8".to_owned())?;
    let rows = rows.lines().collect::<Vec<_>>();
    if rows.len() != updates.len() {
        return Err("batched commit validation returned incomplete results".into());
    }
    for ((reference, expected_oid), row) in updates.iter().zip(rows) {
        let mut fields = row.split_whitespace();
        if fields.next() != Some(expected_oid.as_str()) || fields.next() != Some("commit") {
            return Err(format!("recovery target is not a commit: {reference}"));
        }
    }
    Ok(())
}

fn run_create_only_push(
    remote: &str,
    repo: &std::path::Path,
    updates: &[(String, String)],
    transport: PushTransport,
) -> Result<(), String> {
    let hooks = tempfile::tempdir().map_err(|_| "create isolated Git hook directory")?;
    let hooks_path = hooks
        .path()
        .to_str()
        .ok_or("isolated Git hook directory path is not UTF-8")?;
    if hooks_path.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("isolated Git hook directory path contains control characters".into());
    }
    let mut command = git_repo_command(repo);
    command.args(["-c", "push.followTags=false", "-c"]);
    command
        .arg(format!("core.hooksPath={hooks_path}"))
        .arg("push");
    command.args([
        "--atomic",
        "--porcelain",
        "--no-follow-tags",
        "--no-verify",
        "--no-recurse-submodules",
        "--no-push-option",
    ]);
    for (reference, _) in updates {
        command.arg(format!("--force-with-lease={reference}:"));
    }
    command.arg("--").arg(remote);
    for (reference, oid) in updates {
        command.arg(format!("{oid}:{reference}"));
    }
    command.env("GIT_LFS_SKIP_PUSH", "1");
    apply_safe_transport(&mut command, transport)?;
    let output = command
        .output()
        .map_err(|error| format!("start atomic create-only recovery push: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "atomic create-only recovery push failed (git exit {:?})",
            output.status.code()
        ))
    }
}

fn reconcile_create_only_push(
    remote: &str,
    repo: &std::path::Path,
    updates: &[(String, String)],
    push_result: Result<(), String>,
    transport: PushTransport,
) -> Result<CreateOnlyPushOutcome, String> {
    let observed = match remote_recovery_refs(remote, repo, updates, transport) {
        Ok(observed) => observed,
        Err(error) => {
            return Err(match push_result {
                Ok(()) => format!(
                    "atomic create-only recovery push succeeded but verification failed: {error}"
                ),
                Err(push_error) => format!("{push_error}; remote verification failed: {error}"),
            });
        }
    };
    let mismatches = updates
        .iter()
        .filter_map(|(reference, expected)| match observed.get(reference) {
            Some(actual) if actual == expected => None,
            Some(actual) => Some(format!("{reference} is {actual}, expected {expected}")),
            None => Some(format!("{reference} is missing")),
        })
        .collect::<Vec<_>>();
    if mismatches.is_empty() {
        return Ok(match push_result {
            Ok(()) => CreateOnlyPushOutcome::Acknowledged,
            Err(_) => CreateOnlyPushOutcome::ReconciledAfterError,
        });
    }
    let mismatch = mismatches.join("; ");
    match push_result {
        Ok(()) => Err(format!(
            "atomic create-only recovery push left unexpected refs: {mismatch}"
        )),
        Err(error) => Err(format!("{error}; remote refs not exact: {mismatch}")),
    }
}

fn remote_recovery_refs(
    remote: &str,
    repo: &std::path::Path,
    updates: &[(String, String)],
    transport: PushTransport,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut command = git_repo_command(repo);
    command.args(["ls-remote", "--refs", "--"]).arg(remote);
    for (reference, _) in updates {
        command.arg(reference);
    }
    apply_safe_transport(&mut command, transport)?;
    let output = command
        .output()
        .map_err(|error| format!("start recovery ref verification: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote failed (git exit {:?})",
            output.status.code()
        ));
    }
    let mut observed = std::collections::BTreeMap::new();
    for line in String::from_utf8(output.stdout)
        .map_err(|_| "remote ref verification returned invalid UTF-8".to_owned())?
        .lines()
    {
        let (oid, reference) = line
            .split_once('\t')
            .ok_or("remote ref verification returned malformed output")?;
        if !updates.iter().any(|(requested, _)| requested == reference) {
            continue;
        }
        if let Some(previous) = observed.insert(reference.to_owned(), oid.to_owned())
            && previous != oid
        {
            return Err(format!("remote returned conflicting OIDs for {reference}"));
        }
    }
    Ok(observed)
}

fn apply_safe_transport(command: &mut Command, transport: PushTransport) -> Result<(), String> {
    if transport == PushTransport::GitHubSsh {
        #[cfg(unix)]
        command
            .env("GIT_SSH_COMMAND", SAFE_GITHUB_SSH_COMMAND)
            .env("GIT_SSH_VARIANT", "ssh");
        #[cfg(not(unix))]
        return Err("safe GitHub SSH recovery push is unavailable on this platform".into());
    }
    Ok(())
}

fn git_repo_command(repo: &std::path::Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    command
        .arg("-c")
        .arg(format!("safe.directory={}", repo.display()));
    crate::remote_lfs::sanitize_git_environment(&mut command);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env_remove("GIT_EXEC_PATH");
    command
}

fn git_repo_text(repo: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = git_repo_command(repo)
        .args(args)
        .output()
        .map_err(|error| format!("start git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| format!("git {} returned invalid UTF-8", args.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn command(args: &[&str]) -> String {
        let output = Command::new(args[0]).args(&args[1..]).output().unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn bare_remote() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let bare = temp.path().join("remote.git");
        let work = temp.path().join("work");
        command(&["git", "init", "--bare", bare.to_str().unwrap()]);
        command(&["git", "init", "-b", "main", work.to_str().unwrap()]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "user.name",
            "Fixture",
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        fs::write(work.join("file.txt"), "fixture\n").unwrap();
        command(&["git", "-C", work.to_str().unwrap(), "add", "file.txt"]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "commit",
            "-m",
            "fixture",
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "push",
            bare.to_str().unwrap(),
            "main",
        ]);
        let oid = command(&["git", "-C", work.to_str().unwrap(), "rev-parse", "HEAD"]);
        (temp, bare, oid)
    }

    fn remote_ref_oid(git_dir: &Path, reference: &str) -> String {
        command(&[
            "git",
            "--git-dir",
            git_dir.to_str().unwrap(),
            "rev-parse",
            reference,
        ])
    }

    fn commit_tip(work: &Path, file: &str, body: &str) -> String {
        fs::write(work.join(file), body).unwrap();
        command(&["git", "-C", work.to_str().unwrap(), "add", file]);
        command(&["git", "-C", work.to_str().unwrap(), "commit", "-m", file]);
        command(&["git", "-C", work.to_str().unwrap(), "rev-parse", "HEAD"])
    }

    #[test]
    fn data_batch_uploads_local_only_commits_and_creates_refs() {
        let temp = tempfile::tempdir().unwrap();
        let bare = temp.path().join("fresh-remote.git");
        command(&["git", "init", "--bare", "--quiet", bare.to_str().unwrap()]);
        let work = temp.path().join("work");
        command(&[
            "git",
            "init",
            "--quiet",
            "-b",
            "main",
            work.to_str().unwrap(),
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "user.name",
            "Fixture",
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        let one = commit_tip(&work, "local-only-one.txt", "payload one\n");
        let two = commit_tip(&work, "local-only-two.txt", "payload two\n");
        let updates = vec![
            ("refs/heads/recovery/data/one".to_owned(), one.clone()),
            ("refs/heads/recovery/data/two".to_owned(), two.clone()),
        ];
        assert_eq!(
            push_data_create_only_batch(bare.to_str().unwrap(), &work, &updates).unwrap(),
            CreateOnlyPushOutcome::Acknowledged
        );
        for (reference, oid) in &updates {
            assert_eq!(remote_ref_oid(&bare, reference), *oid);
        }
        command(&[
            "git",
            "--git-dir",
            bare.to_str().unwrap(),
            "cat-file",
            "-e",
            &one,
        ]);
        assert_eq!(
            command(&[
                "git",
                "--git-dir",
                bare.to_str().unwrap(),
                "show",
                "refs/heads/recovery/data/one:local-only-one.txt"
            ]),
            "payload one"
        );
    }

    #[test]
    fn data_batch_collision_is_atomic_and_preserves_existing_ref() {
        let (temp, bare, base) = bare_remote();
        let work = temp.path().join("work");
        let first = commit_tip(&work, "candidate.txt", "candidate\n");
        let collision = "refs/heads/recovery/data/collision";
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "push",
            "--quiet",
            bare.to_str().unwrap(),
            &format!("{base}:{collision}"),
        ]);
        let before = remote_ref_oid(&bare, collision);
        assert_eq!(before, base, "existing target is a fast-forward ancestor");
        let first_ref = "refs/heads/recovery/data/should-not-appear";
        let error = push_data_create_only_batch(
            bare.to_str().unwrap(),
            &work,
            &[
                (first_ref.to_owned(), first.clone()),
                (collision.to_owned(), first),
            ],
        )
        .unwrap_err();
        assert!(error.contains("failed"), "{error}");
        assert_eq!(remote_ref_oid(&bare, collision), before);
        let absent = Command::new("git")
            .args([
                "--git-dir",
                bare.to_str().unwrap(),
                "show-ref",
                "--verify",
                first_ref,
            ])
            .output()
            .unwrap();
        assert!(
            !absent.status.success(),
            "atomic rejection must not create sibling ref"
        );
    }

    #[cfg(unix)]
    #[test]
    fn data_batch_race_create_rejects_whole_atomic_batch() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, bare, base) = bare_remote();
        let work = temp.path().join("work");
        let candidate = commit_tip(&work, "race-candidate.txt", "candidate\n");
        let competing = "refs/heads/recovery/data/race";
        let sibling = "refs/heads/recovery/data/race-sibling";
        let hook = bare.join("hooks/pre-receive");
        let hook_log = temp.path().join("race-hook.log");
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nenv -u GIT_DIR -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES git --git-dir '{}' update-ref '{}' '{}' >'{}' 2>&1\nstatus=$?\nprintf '%s\\n' \"$status\" >> '{}'\nexit \"$status\"\n",
            bare.display(),
            competing,
            base,
            hook_log.display(),
            hook_log.display()
        );
        fs::write(&hook, script).unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();

        let error = push_data_create_only_batch(
            bare.to_str().unwrap(),
            &work,
            &[
                (competing.to_owned(), candidate.clone()),
                (sibling.to_owned(), candidate),
            ],
        )
        .unwrap_err();
        assert!(error.contains("failed"), "{error}");
        assert_eq!(fs::read_to_string(hook_log).unwrap(), "0\n");
        assert_eq!(remote_ref_oid(&bare, competing), base);
        let sibling = Command::new("git")
            .args([
                "--git-dir",
                bare.to_str().unwrap(),
                "show-ref",
                "--verify",
                sibling,
            ])
            .output()
            .unwrap();
        assert!(
            !sibling.status.success(),
            "concurrent ref creation must not allow partial sibling creation"
        );
    }

    #[test]
    fn lost_response_reconciliation_is_not_attributed_as_acknowledged() {
        let (_temp, bare, oid) = bare_remote();
        let reference = "refs/heads/recovery/data/reconciled";
        let zero_oid = "0".repeat(oid.len());
        command(&[
            "git",
            "--git-dir",
            bare.to_str().unwrap(),
            "update-ref",
            reference,
            &oid,
            &zero_oid,
        ]);
        let outcome = reconcile_create_only_push(
            bare.to_str().unwrap(),
            Path::new("."),
            &[(reference.to_owned(), oid)],
            Err("simulated lost push response".into()),
            PushTransport::Other,
        )
        .unwrap();
        assert_eq!(outcome, CreateOnlyPushOutcome::ReconciledAfterError);
    }

    #[test]
    fn push_protocol_sends_zero_old_oid_for_every_create() {
        if let (Ok(work), Ok(remote), Ok(oid), Ok(reference)) = (
            std::env::var("GITHUB_REF_TEST_PACKET_WORK"),
            std::env::var("GITHUB_REF_TEST_PACKET_REMOTE"),
            std::env::var("GITHUB_REF_TEST_PACKET_OID"),
            std::env::var("GITHUB_REF_TEST_PACKET_REF"),
        ) {
            let sibling = format!("{reference}-sibling");
            let outcome = push_data_create_only_batch(
                &remote,
                Path::new(&work),
                &[(reference, oid.clone()), (sibling, oid)],
            )
            .unwrap();
            assert_eq!(outcome, CreateOnlyPushOutcome::Acknowledged);
            return;
        }

        let (temp, bare, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "packet-trace.txt", "payload\n");
        let reference = "refs/heads/recovery/expected-zero";
        let trace = temp.path().join("git-packet-trace.log");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::push_protocol_sends_zero_old_oid_for_every_create",
            ])
            .env("GITHUB_REF_TEST_PACKET_WORK", &work)
            .env("GITHUB_REF_TEST_PACKET_REMOTE", bare.to_str().unwrap())
            .env("GITHUB_REF_TEST_PACKET_OID", &oid)
            .env("GITHUB_REF_TEST_PACKET_REF", reference)
            .env("GIT_TRACE_PACKET", &trace)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let trace = fs::read_to_string(trace).unwrap();
        for reference in [reference, "refs/heads/recovery/expected-zero-sibling"] {
            let expected_update = format!("{} {oid} {reference}", "0".repeat(oid.len()));
            assert!(
                trace.contains(&expected_update),
                "push protocol did not send an expected-old-zero update for {reference}: {trace}"
            );
        }
    }

    #[test]
    fn malformed_batch_is_rejected_before_any_git_command() {
        let missing_repo = Path::new("/path/that/does/not/exist");
        let error = push_data_create_only_batch(
            "/not/a/remote",
            missing_repo,
            &[("refs/tags/recovery/wrong-namespace".into(), "1".repeat(40))],
        )
        .unwrap_err();
        assert!(
            error.contains("invalid create-only recovery update"),
            "{error}"
        );

        let error = push_data_create_only_batch(
            "/not/a/remote",
            missing_repo,
            &[(
                "refs/heads/recovery/malformed-oid".into(),
                "not-an-oid".into(),
            )],
        )
        .unwrap_err();
        assert!(
            error.contains("invalid create-only recovery update"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn data_batch_does_not_run_push_hooks_or_follow_tags() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, bare, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "hook-test.txt", "payload\n");
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "tag",
            "-a",
            "local-only-tag",
            "-m",
            "tag should not follow",
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "push.followTags",
            "true",
        ]);
        let hooks = temp.path().join("push-hooks");
        fs::create_dir_all(&hooks).unwrap();
        let marker = temp.path().join("pre-push-ran");
        let script = format!(
            "#!/bin/sh\nprintf invoked > '{}'\nexit 55\n",
            marker.display()
        );
        let pre_push = hooks.join("pre-push");
        fs::write(&pre_push, script).unwrap();
        let mut permissions = fs::metadata(&pre_push).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&pre_push, permissions).unwrap();
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "core.hooksPath",
            hooks.to_str().unwrap(),
        ]);

        assert_eq!(
            push_data_create_only_batch(
                bare.to_str().unwrap(),
                &work,
                &[("refs/heads/recovery/hook-safe".into(), oid)],
            )
            .unwrap(),
            CreateOnlyPushOutcome::Acknowledged
        );
        assert!(!marker.exists(), "pre-push hook must not run");
        let tag = Command::new("git")
            .args([
                "--git-dir",
                bare.to_str().unwrap(),
                "show-ref",
                "--verify",
                "refs/tags/local-only-tag",
            ])
            .output()
            .unwrap();
        assert!(!tag.status.success(), "annotated tag must not be followed");
    }

    #[cfg(unix)]
    #[test]
    fn data_batch_suppresses_configured_push_options() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, bare, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "push-options.txt", "payload\n");
        command(&[
            "git",
            "--git-dir",
            bare.to_str().unwrap(),
            "config",
            "receive.advertisePushOptions",
            "true",
        ]);
        let hook = bare.join("hooks/pre-receive");
        let marker = temp.path().join("received-push-options");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$GIT_PUSH_OPTION_COUNT\" \"${{GIT_PUSH_OPTION_0-}}\" > '{}'\n",
            marker.display()
        );
        fs::write(&hook, script).unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "--add",
            "push.pushOption",
            "deploy=unsafe-test",
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "--add",
            "push.pushOption",
            "release=unsafe-test",
        ]);

        let error = push_data_create_only_batch(
            bare.to_str().unwrap(),
            &work,
            &[("refs/heads/recovery/no-push-options".into(), oid)],
        )
        .unwrap_err();
        assert!(error.contains("configured Git push options"), "{error}");
        assert!(
            !marker.exists(),
            "reject push options before contacting the receiver"
        );
        let absent = Command::new("git")
            .args([
                "--git-dir",
                bare.to_str().unwrap(),
                "show-ref",
                "--verify",
                "refs/heads/recovery/no-push-options",
            ])
            .output()
            .unwrap();
        assert!(!absent.status.success(), "rejected push must create no ref");
    }

    #[test]
    fn data_batch_never_pushes_submodule_commits_from_config() {
        let (temp, bare, _) = bare_remote();
        let work = temp.path().join("work");
        let sub_bare = temp.path().join("submodule.git");
        let sub_work = temp.path().join("submodule-work");
        command(&[
            "git",
            "init",
            "--bare",
            "--quiet",
            sub_bare.to_str().unwrap(),
        ]);
        command(&[
            "git",
            "init",
            "--quiet",
            "-b",
            "main",
            sub_work.to_str().unwrap(),
        ]);
        command(&[
            "git",
            "-C",
            sub_work.to_str().unwrap(),
            "config",
            "user.name",
            "Fixture",
        ]);
        command(&[
            "git",
            "-C",
            sub_work.to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        fs::write(sub_work.join("base.txt"), "submodule base\n").unwrap();
        command(&["git", "-C", sub_work.to_str().unwrap(), "add", "base.txt"]);
        command(&[
            "git",
            "-C",
            sub_work.to_str().unwrap(),
            "commit",
            "-m",
            "submodule base",
        ]);
        command(&[
            "git",
            "-C",
            sub_work.to_str().unwrap(),
            "push",
            "--quiet",
            sub_bare.to_str().unwrap(),
            "main",
        ]);

        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            sub_bare.to_str().unwrap(),
            "nested",
        ]);
        command(&[
            "git",
            "-C",
            work.join("nested").to_str().unwrap(),
            "config",
            "user.name",
            "Fixture",
        ]);
        command(&[
            "git",
            "-C",
            work.join("nested").to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        let new_submodule_oid = commit_tip(&work.join("nested"), "local-only.txt", "unpublished\n");
        command(&["git", "-C", work.to_str().unwrap(), "add", "nested"]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "commit",
            "-m",
            "update submodule pointer",
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "push.recurseSubmodules",
            "on-demand",
        ]);
        let superproject_oid = command(&["git", "-C", work.to_str().unwrap(), "rev-parse", "HEAD"]);

        assert_eq!(
            push_data_create_only_batch(
                bare.to_str().unwrap(),
                &work,
                &[(
                    "refs/heads/recovery/no-submodule-push".into(),
                    superproject_oid
                )],
            )
            .unwrap(),
            CreateOnlyPushOutcome::Acknowledged
        );
        let absent = Command::new("git")
            .args([
                "--git-dir",
                sub_bare.to_str().unwrap(),
                "cat-file",
                "-e",
                &new_submodule_oid,
            ])
            .output()
            .unwrap();
        assert!(
            !absent.status.success(),
            "push.recurseSubmodules must not upload unscanned submodule commits"
        );
    }

    #[cfg(unix)]
    #[test]
    fn data_batch_rejects_ssh_override_before_git_subprocess() {
        if let (Ok(work), Ok(remote), Ok(oid)) = (
            std::env::var("GITHUB_REF_TEST_SSH_WORK"),
            std::env::var("GITHUB_REF_TEST_SSH_REMOTE"),
            std::env::var("GITHUB_REF_TEST_SSH_OID"),
        ) {
            let error = push_data_create_only_batch(
                &remote,
                Path::new(&work),
                &[("refs/heads/recovery/ssh-guard".into(), oid)],
            )
            .unwrap_err();
            assert!(
                error.contains("custom SSH or proxy configuration"),
                "{error}"
            );
            return;
        }

        let (temp, bare, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "ssh-guard.txt", "payload\n");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::data_batch_rejects_ssh_override_before_git_subprocess",
            ])
            .env("GITHUB_REF_TEST_SSH_WORK", &work)
            .env("GITHUB_REF_TEST_SSH_REMOTE", bare.to_str().unwrap())
            .env("GITHUB_REF_TEST_SSH_OID", oid)
            .env("GIT_SSH_COMMAND", "custom-ssh-wrapper")
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn data_batch_rejects_named_remote_transport_commands_before_execution() {
        use std::os::unix::fs::PermissionsExt;

        for override_name in ["uploadpack", "receivepack"] {
            let (temp, bare, _) = bare_remote();
            let work = temp.path().join("work");
            let oid = commit_tip(&work, "remote-command.txt", "payload\n");
            command(&[
                "git",
                "-C",
                work.to_str().unwrap(),
                "remote",
                "add",
                "origin",
                bare.to_str().unwrap(),
            ]);

            let marker = temp.path().join(format!("{override_name}-ran"));
            let wrapper = temp.path().join(format!("{override_name}-wrapper"));
            let script = format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 1\n",
                marker.display()
            );
            fs::write(&wrapper, script).unwrap();
            let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&wrapper, permissions).unwrap();
            command(&[
                "git",
                "-C",
                work.to_str().unwrap(),
                "config",
                &format!("remote.origin.{override_name}"),
                wrapper.to_str().unwrap(),
            ]);

            let error = push_data_create_only_batch(
                "origin",
                &work,
                &[("refs/heads/recovery/transport-command".into(), oid)],
            )
            .unwrap_err();
            assert!(error.contains("custom Git transport command"), "{error}");
            assert!(!marker.exists(), "configured {override_name} command ran");
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepts_only_canonical_github_ssh_transport_urls() {
        for remote in [
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "ssh://git@github.com:22/owner/repo",
        ] {
            assert_eq!(
                validate_remote_transport(Path::new("."), remote).unwrap(),
                PushTransport::GitHubSsh,
                "{remote}"
            );
        }
        for remote in [
            "git@github-work:owner/repo.git",
            "git@attacker.example:owner/repo.git",
            "ssh://git@github.com:2222/owner/repo.git",
            "ssh://git@github.com.evil/owner/repo.git",
            "ssh://alice@github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git?redirect=elsewhere",
            "git+ssh://git@github.com/owner/repo.git",
        ] {
            assert!(
                validate_remote_transport(Path::new("."), remote).is_err(),
                "accepted unsafe SSH URL: {remote}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn github_ssh_push_ignores_user_ssh_config_redirections() {
        if let (Ok(work), Ok(remote), Ok(oid)) = (
            std::env::var("GITHUB_REF_TEST_SSH_CONFIG_WORK"),
            std::env::var("GITHUB_REF_TEST_SSH_CONFIG_REMOTE"),
            std::env::var("GITHUB_REF_TEST_SSH_CONFIG_OID"),
        ) {
            let result = push_data_create_only_batch(
                &remote,
                Path::new(&work),
                &[("refs/heads/recovery/ssh-config-safe".into(), oid)],
            );
            assert!(
                result.is_err(),
                "test SSH wrapper should reject the connection"
            );
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let (temp, _, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "ssh-config.txt", "payload\n");
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "git@github.com:owner/repo.git",
        ]);

        let home = temp.path().join("home");
        let ssh_dir = home.join(".ssh");
        fs::create_dir_all(&ssh_dir).unwrap();
        let match_marker = temp.path().join("ssh-match-exec-ran");
        let proxy_marker = temp.path().join("ssh-proxy-command-ran");
        let ssh_config = format!(
            "Match exec \"/bin/touch {}\"\n    HostName 127.0.0.1\n\nHost github.com\n    ProxyCommand /bin/touch {}\n",
            match_marker.display(),
            proxy_marker.display()
        );
        fs::write(ssh_dir.join("config"), ssh_config).unwrap();

        let mut ssh_words = SAFE_GITHUB_SSH_COMMAND.split_whitespace();
        let ssh_program = ssh_words.next().unwrap();
        let ssh_config = Command::new(ssh_program)
            .args(ssh_words)
            .args(["-G", "git@github.com"])
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(
            ssh_config.status.success(),
            "safe SSH config inspection failed: {}",
            String::from_utf8_lossy(&ssh_config.stderr)
        );
        let ssh_config = String::from_utf8_lossy(&ssh_config.stdout);
        assert!(ssh_config.lines().any(|line| line == "hostname github.com"));
        assert!(
            ssh_config
                .lines()
                .filter(|line| line.starts_with("proxycommand "))
                .all(|line| line == "proxycommand none"),
            "user ProxyCommand must be absent or disabled"
        );
        assert!(
            ssh_config
                .lines()
                .filter(|line| line.starts_with("proxyjump "))
                .all(|line| line == "proxyjump none"),
            "user ProxyJump must be absent or disabled"
        );
        assert!(!match_marker.exists(), "OpenSSH Match exec ran");
        assert!(!proxy_marker.exists(), "OpenSSH ProxyCommand ran");

        let fake_bin = temp.path().join("bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let ssh_log = temp.path().join("ssh-wrapper-args");
        let wrapper = fake_bin.join("ssh");
        fs::write(
            &wrapper,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$SSH_WRAPPER_LOG\"\nexit 71\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper, permissions).unwrap();
        let mut paths = vec![fake_bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).unwrap();

        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::github_ssh_push_ignores_user_ssh_config_redirections",
            ])
            .env("GITHUB_REF_TEST_SSH_CONFIG_WORK", &work)
            .env("GITHUB_REF_TEST_SSH_CONFIG_REMOTE", "origin")
            .env("GITHUB_REF_TEST_SSH_CONFIG_OID", &oid)
            .env("SSH_WRAPPER_LOG", &ssh_log)
            .env("HOME", &home)
            .env("PATH", path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_SSH")
            .env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_PROXY_COMMAND")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let wrapper_args = fs::read_to_string(ssh_log).unwrap();
        assert!(wrapper_args.contains("-F\n/dev/null\n"), "{wrapper_args}");
        assert!(
            wrapper_args.contains("ProxyCommand=none") && wrapper_args.contains("ProxyJump=none"),
            "{wrapper_args}"
        );
        assert!(!match_marker.exists(), "OpenSSH Match exec ran");
        assert!(!proxy_marker.exists(), "OpenSSH ProxyCommand ran");
    }

    #[cfg(unix)]
    #[test]
    fn https_push_rejects_repository_credential_helpers() {
        let (temp, _, _) = bare_remote();
        let work = temp.path().join("work");
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "https://127.0.0.1:1/owner/repo.git",
        ]);
        let marker = temp.path().join("credential-helper-ran");
        let helper = format!("!touch {}", marker.display());
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "credential.helper",
            &helper,
        ]);
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "config",
            "credential.https://127.0.0.1.helper",
            &helper,
        ]);

        let error = validate_push_remote(&work, "origin").unwrap_err();
        assert!(error.contains("repository credential helper"), "{error}");
        assert!(!marker.exists(), "repository credential helper ran");
    }

    #[test]
    fn https_remote_allows_user_level_credential_helpers() {
        if let Ok(work) = std::env::var("GITHUB_REF_TEST_GLOBAL_HELPER_WORK") {
            assert_eq!(
                validate_push_remote(Path::new(&work), "origin").unwrap(),
                PushTransport::Https
            );
            return;
        }

        let (temp, _, _) = bare_remote();
        let work = temp.path().join("work");
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "https://127.0.0.1:1/owner/repo.git",
        ]);
        let home = temp.path().join("trusted-home");
        fs::create_dir_all(&home).unwrap();
        let global_config = home.join(".gitconfig");
        let marker = temp.path().join("trusted-user-helper-ran");
        let helper = format!("!touch {}", marker.display());
        command(&[
            "git",
            "config",
            "--file",
            global_config.to_str().unwrap(),
            "credential.helper",
            &helper,
        ]);
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::https_remote_allows_user_level_credential_helpers",
            ])
            .env("GITHUB_REF_TEST_GLOBAL_HELPER_WORK", &work)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(!marker.exists(), "inspection must not invoke user helper");
    }

    #[cfg(unix)]
    #[test]
    fn data_batch_ignores_inherited_git_exec_path() {
        if let (Ok(work), Ok(remote), Ok(oid), Ok(marker)) = (
            std::env::var("GITHUB_REF_TEST_EXEC_PATH_WORK"),
            std::env::var("GITHUB_REF_TEST_EXEC_PATH_REMOTE"),
            std::env::var("GITHUB_REF_TEST_EXEC_PATH_OID"),
            std::env::var("GITHUB_REF_TEST_EXEC_PATH_MARKER"),
        ) {
            let result = push_data_create_only_batch(
                &remote,
                Path::new(&work),
                &[("refs/heads/recovery/safe-exec-path".into(), oid)],
            );
            assert!(result.is_err(), "local test endpoint should fail closed");
            assert!(!Path::new(&marker).exists(), "inherited Git helper ran");
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let (temp, _, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "exec-path.txt", "payload\n");
        command(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "https://127.0.0.1:1/owner/repo.git",
        ]);
        let exec_path = temp.path().join("untrusted-git-exec");
        fs::create_dir_all(&exec_path).unwrap();
        let marker = temp.path().join("git-remote-https-ran");
        let script = "#!/bin/sh\ntouch \"$GITHUB_REF_TEST_EXEC_PATH_MARKER\"\nexit 73\n";
        for helper_name in ["git-remote-https", "git-remote-http"] {
            let helper = exec_path.join(helper_name);
            fs::write(&helper, script).unwrap();
            let mut permissions = fs::metadata(&helper).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&helper, permissions).unwrap();
        }
        let home = temp.path().join("isolated-home");
        fs::create_dir_all(&home).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::data_batch_ignores_inherited_git_exec_path",
            ])
            .env("GITHUB_REF_TEST_EXEC_PATH_WORK", &work)
            .env("GITHUB_REF_TEST_EXEC_PATH_REMOTE", "origin")
            .env("GITHUB_REF_TEST_EXEC_PATH_OID", &oid)
            .env("GITHUB_REF_TEST_EXEC_PATH_MARKER", &marker)
            .env("GIT_EXEC_PATH", &exec_path)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HTTP_PROXY", "")
            .env("HTTPS_PROXY", "")
            .env("ALL_PROXY", "")
            .env("http_proxy", "")
            .env("https_proxy", "")
            .env("all_proxy", "")
            .env_remove("GIT_SSH")
            .env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_PROXY_COMMAND")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(!marker.exists(), "inherited Git helper ran");
    }

    #[test]
    fn data_batch_errors_redact_authenticated_remote_urls() {
        if let (Ok(work), Ok(oid), Ok(remote)) = (
            std::env::var("GITHUB_REF_TEST_AUTH_WORK"),
            std::env::var("GITHUB_REF_TEST_AUTH_OID"),
            std::env::var("GITHUB_REF_TEST_AUTH_REMOTE"),
        ) {
            let error = push_data_create_only_batch(
                &remote,
                Path::new(&work),
                &[("refs/heads/recovery/redaction".into(), oid)],
            )
            .unwrap_err();
            assert!(!error.contains("sentinel-secret"), "{error}");
            assert!(!error.contains("credential-user"), "{error}");
            assert!(!error.contains(&remote), "{error}");
            return;
        }

        let (temp, _bare, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "credential-redaction.txt", "payload\n");
        let authenticated_remote =
            "https://credential-user:sentinel-secret@127.0.0.1:1/recovery.git";
        let home = temp.path().join("isolated-home");
        fs::create_dir_all(&home).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::data_batch_errors_redact_authenticated_remote_urls",
            ])
            .env("GITHUB_REF_TEST_AUTH_WORK", &work)
            .env("GITHUB_REF_TEST_AUTH_OID", &oid)
            .env("GITHUB_REF_TEST_AUTH_REMOTE", authenticated_remote)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HTTP_PROXY", "")
            .env("HTTPS_PROXY", "")
            .env("ALL_PROXY", "")
            .env("http_proxy", "")
            .env("https_proxy", "")
            .env("all_proxy", "")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[test]
    fn data_batch_ignores_hostile_inherited_git_environment() {
        if let (Ok(work), Ok(remote), Ok(oid)) = (
            std::env::var("GITHUB_REF_TEST_WORK"),
            std::env::var("GITHUB_REF_TEST_REMOTE"),
            std::env::var("GITHUB_REF_TEST_OID"),
        ) {
            push_data_create_only_batch(
                &remote,
                Path::new(&work),
                &[("refs/heads/recovery/sanitized".into(), oid)],
            )
            .unwrap();
            return;
        }
        let (temp, bare, _) = bare_remote();
        let work = temp.path().join("work");
        let oid = commit_tip(&work, "sanitized.txt", "payload\n");
        let hostile = temp.path().join("hostile.git");
        command(&[
            "git",
            "init",
            "--bare",
            "--quiet",
            hostile.to_str().unwrap(),
        ]);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "github_ref::tests::data_batch_ignores_hostile_inherited_git_environment",
            ])
            .env("GITHUB_REF_TEST_WORK", &work)
            .env("GITHUB_REF_TEST_REMOTE", bare.to_str().unwrap())
            .env("GITHUB_REF_TEST_OID", oid.clone())
            .env("GIT_DIR", &hostile)
            .env("GIT_WORK_TREE", temp.path())
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "remote.origin.url")
            .env("GIT_CONFIG_VALUE_0", hostile.to_str().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(remote_ref_oid(&bare, "refs/heads/recovery/sanitized"), oid);
        let hostile_refs = Command::new("git")
            .args(["--git-dir", hostile.to_str().unwrap(), "show-ref"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&hostile_refs.stdout)
                .trim()
                .is_empty()
        );
    }
}
