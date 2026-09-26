//! Create a GitHub branch ref without updating an existing ref.
//!
//! This module deliberately has no Git-push fallback. GitHub's create-ref API
//! rejects an existing name; callers must ensure `oid` already exists remotely
//! (for example, by selecting an existing remote ancestor), then may use a
//! normal fast-forward push to advance the newly created ref.

use std::{
    path::PathBuf,
    process::{Command, Output},
};

/// Parse a GitHub.com clone URL into its `owner/repository` API path.
pub fn repository_nwo(remote_url: &str) -> Result<String, String> {
    let value = remote_url.trim();
    let path = if let Some((user_host, path)) = value.split_once(':') {
        if user_host.eq_ignore_ascii_case("git@github.com") {
            path
        } else {
            parse_github_url(value)?
        }
    } else {
        parse_github_url(value)?
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let pieces = path.split('/').collect::<Vec<_>>();
    if pieces.len() != 2
        || pieces.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || part.contains(['?', '#', ':', '@', '%'])
        })
    {
        return Err("GitHub URL must contain exactly an owner and repository".into());
    }
    Ok(format!("{}/{}", pieces[0], pieces[1]))
}

fn parse_github_url(value: &str) -> Result<&str, String> {
    let (scheme, rest) = value
        .split_once("://")
        .ok_or_else(|| "atomic create-ref API is supported only for github.com".to_owned())?;
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| "GitHub clone URL is missing owner/repository".to_owned())?;
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    if user.is_some_and(|user| user != "git") {
        return Err("GitHub SSH URL must use the git user".into());
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (host, Some(port))
        }
        _ => (host_port, None),
    };
    let allowed = match scheme.as_str() {
        "https" => user.is_none() && (port.is_none() || port == Some("443")),
        "ssh" => user == Some("git") && (port.is_none() || port == Some("22")),
        _ => false,
    };
    if !allowed || !host.eq_ignore_ascii_case("github.com") {
        return Err("atomic create-ref API is supported only for github.com".into());
    }
    if path.contains(['?', '#']) {
        return Err("GitHub clone URL must not contain a query or fragment".into());
    }
    Ok(path)
}

fn validate_ref_and_oid(reference: &str, oid: &str) -> Result<(), String> {
    let name = reference
        .strip_prefix("refs/heads/")
        .ok_or_else(|| "create-ref requires a full refs/heads name".to_owned())?;
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name
            .bytes()
            .any(|byte| byte <= b' ' || b"~^:?*[\\".contains(&byte) || byte == 0x7f)
        || name.split('/').any(|part| part.ends_with(".lock"))
    {
        return Err("invalid recovery branch name".into());
    }
    if (oid.len() != 40 && oid.len() != 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("expected a full 40- or 64-character commit object ID".into());
    }
    Ok(())
}

fn request_args(nwo: &str, reference: &str, oid: &str) -> Vec<String> {
    vec![
        "gh".into(),
        "api".into(),
        "--hostname".into(),
        "github.com".into(),
        "--method".into(),
        "POST".into(),
        format!("repos/{nwo}/git/refs"),
        "--field".into(),
        format!("ref={reference}"),
        "--field".into(),
        format!("sha={oid}"),
    ]
}

/// Create a ref using GitHub's create-only REST endpoint.
///
/// GitHub returns an error when the ref already exists. Errors are surfaced to
/// the caller; this function never probes for a fallback push or updates refs.
pub fn create_ref(remote_url: &str, reference: &str, oid: &str) -> Result<(), String> {
    validate_ref_and_oid(reference, oid)?;
    if let Some(git_dir) = explicit_local_git_dir(remote_url)? {
        return create_local_ref(&git_dir, reference, oid);
    }
    create_ref_with(remote_url, reference, oid, |args| {
        let output = Command::new(&args[0])
            .args(&args[1..])
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_HOST", "github.com")
            .output()
            .map_err(|error| format!("could not start gh create-ref request: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "gh create-ref request failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    })
}

fn explicit_local_git_dir(remote: &str) -> Result<Option<PathBuf>, String> {
    let path = if let Some(path) = remote.strip_prefix("file://") {
        if !path.starts_with('/') || path.starts_with("//") || path.contains('%') {
            return Err("file remote must use a local absolute path without URL encoding".into());
        }
        Some(PathBuf::from(path))
    } else {
        let path = PathBuf::from(remote);
        if path.is_absolute() || path.exists() {
            Some(path)
        } else {
            None
        }
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("local Git remote is inaccessible: {error}"))?;
    Ok(Some(canonical))
}

fn git_output(args: &[&str]) -> Result<Output, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("could not start local git command: {error}"))?;
    Ok(output)
}

fn checked_stdout(args: &[&str]) -> Result<String, String> {
    let output = git_output(args)?;
    if !output.status.success() {
        return Err(format!(
            "local git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn create_local_ref(git_dir: &std::path::Path, reference: &str, oid: &str) -> Result<(), String> {
    let git_dir = git_dir
        .to_str()
        .ok_or_else(|| "local Git remote path is not UTF-8".to_owned())?;
    let bare = checked_stdout(&["-C", git_dir, "rev-parse", "--is-bare-repository"])
        .map_err(|_| "local create-ref destination is not a Git repository".to_owned())?;
    if bare != "true" {
        return Err("local create-ref destination is not a bare repository".into());
    }
    let format = checked_stdout(&["--git-dir", git_dir, "rev-parse", "--show-object-format"])?;
    let oid_len = match format.as_str() {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err(format!("unsupported local Git object format: {format}")),
    };
    if oid.len() != oid_len {
        return Err(format!("commit OID length does not match {format} remote"));
    }
    let commit_spec = format!("{oid}^{{commit}}");
    checked_stdout(&["--git-dir", git_dir, "cat-file", "-e", &commit_spec])?;
    checked_stdout(&["check-ref-format", reference])?;
    let zero_oid = "0".repeat(oid_len);
    let output = git_output(&[
        "--git-dir",
        git_dir,
        "update-ref",
        reference,
        oid,
        &zero_oid,
    ])?;
    if !output.status.success() {
        return Err(format!(
            "local create-ref rejected existing or concurrently created ref {reference}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn create_ref_with<F>(
    remote_url: &str,
    reference: &str,
    oid: &str,
    mut request: F,
) -> Result<(), String>
where
    F: FnMut(&[String]) -> Result<(), String>,
{
    validate_ref_and_oid(reference, oid)?;
    let nwo = repository_nwo(remote_url)?;
    request(&request_args(&nwo, reference, oid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    const OID: &str = "0123456789012345678901234567890123456789";

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

    #[test]
    fn parses_only_supported_github_clone_urls() {
        for url in [
            "https://github.com/ChainArgos/java-monorepo.git",
            "https://GITHUB.com:443/ChainArgos/java-monorepo.git",
            "git@github.com:ChainArgos/java-monorepo.git",
            "ssh://git@github.com/ChainArgos/java-monorepo.git",
            "ssh://git@GITHUB.com:22/ChainArgos/java-monorepo.git",
        ] {
            assert_eq!(repository_nwo(url).unwrap(), "ChainArgos/java-monorepo");
        }
        for url in [
            "https://github.example/owner/repo.git",
            "https://github.com/owner/repo/extra",
            "https://github.com/owner/repo?redirect=elsewhere",
            "https://github.com:444/owner/repo.git",
            "ssh://git@github.com:2222/owner/repo.git",
            "ssh://alice@github.com/owner/repo.git",
        ] {
            assert!(repository_nwo(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn fake_api_creates_absent_ref_and_rejects_existing_without_mutation() {
        let mut refs = BTreeMap::new();
        let reference = "refs/heads/recovery/test-create-only";
        let args_seen = std::cell::RefCell::new(Vec::new());
        let create = |refs: &mut BTreeMap<String, String>, args: &[String]| {
            assert_eq!(args[0], "gh");
            assert_eq!(args[1], "api");
            assert_eq!(args[2], "--hostname");
            assert_eq!(args[3], "github.com");
            assert_eq!(args[4], "--method");
            assert_eq!(args[5], "POST");
            assert_eq!(args[6], "repos/ChainArgos/java-monorepo/git/refs");
            let ref_value = args[8].strip_prefix("ref=").unwrap().to_owned();
            let oid_value = args[10].strip_prefix("sha=").unwrap().to_owned();
            if refs.contains_key(&ref_value) {
                return Err("422 Reference already exists".into());
            }
            refs.insert(ref_value, oid_value);
            Ok(())
        };

        create_ref_with(
            "https://github.com/ChainArgos/java-monorepo.git",
            reference,
            OID,
            |args| {
                args_seen.borrow_mut().push(args.to_vec());
                create(&mut refs, args)
            },
        )
        .unwrap();
        assert_eq!(refs.get(reference).map(String::as_str), Some(OID));
        assert_eq!(args_seen.borrow().len(), 1);

        let before = refs.clone();
        let error = create_ref_with(
            "https://github.com/ChainArgos/java-monorepo.git",
            reference,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            |args| {
                args_seen.borrow_mut().push(args.to_vec());
                create(&mut refs, args)
            },
        )
        .unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(refs, before, "API rejection must not change existing ref");
        assert_eq!(args_seen.borrow().len(), 2, "there is no fallback request");
    }

    #[test]
    fn invalid_reference_or_oid_never_calls_api() {
        let calls = std::cell::Cell::new(0);
        assert!(
            create_ref_with(
                "https://github.com/owner/repo",
                "refs/tags/not-a-branch",
                OID,
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            )
            .is_err()
        );
        assert!(
            create_ref_with(
                "https://github.com/owner/repo",
                "refs/heads/recovery/test",
                "not-an-oid",
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn local_bare_remote_create_ref_is_create_only() {
        let (_temp, bare, oid) = bare_remote();
        let reference = "refs/heads/recovery/local-create-only";
        create_ref(bare.to_str().unwrap(), reference, &oid).unwrap();
        assert_eq!(remote_ref_oid(&bare, reference), oid);

        let before = remote_ref_oid(&bare, reference);
        let error = create_ref(bare.to_str().unwrap(), reference, &oid).unwrap_err();
        assert!(error.contains("rejected existing"), "{error}");
        assert_eq!(remote_ref_oid(&bare, reference), before);
    }

    #[test]
    fn file_url_bare_remote_uses_local_create_only_path() {
        let (_temp, bare, oid) = bare_remote();
        let file_url = format!("file://{}", bare.display());
        let reference = "refs/heads/recovery/file-url";
        create_ref(&file_url, reference, &oid).unwrap();
        assert_eq!(remote_ref_oid(&bare, reference), oid);
    }

    #[test]
    fn non_bare_local_path_is_rejected_without_creating_a_ref() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        command(&["git", "init", "-b", "main", work.to_str().unwrap()]);
        let error = create_ref(
            work.to_str().unwrap(),
            "refs/heads/recovery/should-not-exist",
            OID,
        )
        .unwrap_err();
        assert!(error.contains("not a bare repository"), "{error}");
    }
}
