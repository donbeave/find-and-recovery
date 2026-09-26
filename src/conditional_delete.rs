//! Conditional deletion of a duplicate remote branch.
//!
//! The keeper and candidate tips are checked against the caller's scan OIDs.
//! The push uses explicit leases for both refs and an atomic ref transaction.
//! Git may omit an unchanged keeper from the server-side transaction; tests
//! document the resulting limit on keeper race protection.

use std::path::Path;
use std::process::Command;

/// Delete `candidate_ref` only if its tip and `keeper_ref` still match the
/// caller's observed OIDs.
///
/// Both refs must be full refs (for example, `refs/heads/topic`). A fresh
/// temporary bare repository fetches the expected keeper tip, then submits an
/// atomic push containing a same-tip keeper refspec and candidate deletion.
/// `--force-with-lease` is used only as an expected-old-OID guard; no ref is
/// force-updated. The candidate deletion is guarded by the server's expected
/// old OID. The keeper lease guards the tip advertised during push negotiation;
/// Git may omit a same-tip keeper refspec from the server transaction, so this
/// does not guarantee rejection if the keeper moves after advertisement.
/// Errors fail closed unless the remote changes concurrently.
pub fn delete_if_unchanged(
    remote: &str,
    candidate_ref: &str,
    candidate_oid: &str,
    keeper_ref: &str,
    keeper_oid: &str,
) -> Result<(), String> {
    if candidate_ref == keeper_ref {
        return Err("candidate and keeper refs must differ".into());
    }
    validate_oid(candidate_oid)?;
    validate_oid(keeper_oid)?;
    validate_ref(candidate_ref)?;
    validate_ref(keeper_ref)?;

    let isolated = tempfile::Builder::new()
        .prefix("find-and-recovery-conditional-delete-")
        .tempdir()
        .map_err(|e| format!("create isolated push context: {e}"))?;
    git(None, &["init", "--bare", "-q", path_arg(isolated.path())])?;

    // Fetch only the keeper into an isolated object database. Its exact OID is
    // checked before it is used as the source of the same-tip refspec.
    let keeper_source = "refs/expected/keeper";
    let fetch_refspec = format!("+{keeper_ref}:{keeper_source}");
    git(
        Some(isolated.path()),
        &["fetch", "--no-tags", remote, &fetch_refspec],
    )?;
    let fetched_keeper = git(
        Some(isolated.path()),
        &["rev-parse", "--verify", keeper_source],
    )?;
    if fetched_keeper.trim() != keeper_oid {
        return Err(format!(
            "keeper tip changed before conditional delete: expected {keeper_oid}, found {}",
            fetched_keeper.trim()
        ));
    }

    let candidate_lease = format!("--force-with-lease={candidate_ref}:{candidate_oid}");
    let keeper_lease = format!("--force-with-lease={keeper_ref}:{keeper_oid}");
    let keeper_update = format!("{keeper_source}:{keeper_ref}");
    let candidate_delete = format!(":{candidate_ref}");
    git(
        Some(isolated.path()),
        &[
            "push",
            "--atomic",
            &candidate_lease,
            &keeper_lease,
            remote,
            &keeper_update,
            &candidate_delete,
        ],
    )?;
    Ok(())
}

fn validate_oid(oid: &str) -> Result<(), String> {
    if (oid.len() != 40 && oid.len() != 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid expected object ID: {oid}"));
    }
    Ok(())
}

fn validate_ref(reference: &str) -> Result<(), String> {
    if !reference.starts_with("refs/heads/") {
        return Err(format!("expected a branch ref, got: {reference}"));
    }
    let mut command = git_command(None);
    let result = command
        .args(["check-ref-format", reference])
        .output()
        .map_err(|e| format!("run git check-ref-format: {e}"))?;
    if !result.status.success() {
        return Err(format!("invalid branch ref: {reference}"));
    }
    Ok(())
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("temporary path must be valid UTF-8")
}

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = git_command(cwd);
    let output = command
        .args(args)
        .output()
        .map_err(|e| format!("could not start git {}: {e}", safe_verb(args)))?;
    if !output.status.success() {
        // Fetch/push diagnostics commonly echo remote URLs and credentials.
        if matches!(args.first(), Some(&"fetch") | Some(&"push")) {
            return Err(format!(
                "git {} failed ({})",
                safe_verb(args),
                output.status
            ));
        }
        return Err(format!(
            "git {} failed ({}): {}",
            safe_verb(args),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_command(cwd: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
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
        ) || name.starts_with("GIT_CONFIG_")
        {
            command.env_remove(key);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn safe_verb<'a>(args: &[&'a str]) -> &'a str {
    args.first().copied().unwrap_or("command")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct Fixture {
        _temp: tempfile::TempDir,
        remote: PathBuf,
        work: PathBuf,
        candidate_oid: String,
        keeper_oid: String,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let remote = temp.path().join("remote.git");
            let work = temp.path().join("work");
            run(None, &["init", "--bare", "-q", path_arg(&remote)]);
            run(None, &["init", "-q", path_arg(&work)]);
            run(Some(&work), &["config", "user.name", "fixture"]);
            run(
                Some(&work),
                &["config", "user.email", "fixture@example.invalid"],
            );
            fs::write(work.join("file.txt"), "same content\n").unwrap();
            run(Some(&work), &["add", "file.txt"]);
            run(Some(&work), &["commit", "-m", "fixture"]);
            run(Some(&work), &["branch", "keeper"]);
            run(Some(&work), &["branch", "candidate"]);
            run(Some(&work), &["remote", "add", "origin", path_arg(&remote)]);
            run(
                Some(&work),
                &["push", "-q", "origin", "HEAD:refs/heads/keeper"],
            );
            run(
                Some(&work),
                &["push", "-q", "origin", "HEAD:refs/heads/candidate"],
            );
            let candidate_oid = output(Some(&work), &["rev-parse", "HEAD"]);
            let keeper_oid = candidate_oid.clone();
            Self {
                _temp: temp,
                remote,
                work,
                candidate_oid,
                keeper_oid,
            }
        }

        fn remote_oid(&self, reference: &str) -> Option<String> {
            let result = Command::new("git")
                .args(["--git-dir"])
                .arg(&self.remote)
                .args(["rev-parse", "--verify", reference])
                .output()
                .unwrap();
            result
                .status
                .success()
                .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
        }

        fn advance_branch(&self, branch: &str, content: &str) -> String {
            run(Some(&self.work), &["checkout", "-q", "--detach", "HEAD"]);
            fs::write(self.work.join("file.txt"), content).unwrap();
            run(Some(&self.work), &["commit", "-am", "advance"]);
            let oid = output(Some(&self.work), &["rev-parse", "HEAD"]);
            run(
                Some(&self.work),
                &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
            );
            oid
        }
    }

    fn output(cwd: Option<&Path>, args: &[&str]) -> String {
        git(cwd, args).unwrap().trim().to_owned()
    }

    fn run(cwd: Option<&Path>, args: &[&str]) {
        git(cwd, args).unwrap();
    }

    #[test]
    fn deletes_candidate_when_both_tips_match() {
        let fixture = Fixture::new();
        delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        )
        .unwrap();
        assert_eq!(fixture.remote_oid("refs/heads/candidate"), None);
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(fixture.keeper_oid.as_str())
        );
    }

    #[test]
    fn stale_candidate_lease_keeps_both_remote_refs_unchanged() {
        let fixture = Fixture::new();
        let moved_candidate = fixture.advance_branch("candidate", "candidate moved\n");
        let result = delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        );
        assert!(result.is_err());
        assert_eq!(
            fixture.remote_oid("refs/heads/candidate").as_deref(),
            Some(moved_candidate.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(fixture.keeper_oid.as_str())
        );
    }

    #[test]
    fn stale_keeper_lease_keeps_candidate_and_moved_keeper_unchanged() {
        let fixture = Fixture::new();
        let moved_keeper = fixture.advance_branch("keeper", "keeper moved\n");
        let result = delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        );
        assert!(result.is_err());
        assert_eq!(
            fixture.remote_oid("refs/heads/candidate").as_deref(),
            Some(fixture.candidate_oid.as_str())
        );
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(moved_keeper.as_str())
        );
    }

    #[test]
    fn keeper_move_after_advertisement_is_not_server_guarded() {
        let fixture = Fixture::new();
        let moved_keeper = fixture.advance_branch("race-target", "keeper moves in hook\n");
        let hook = fixture.remote.join("hooks").join("pre-receive");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nset -eu\ngit update-ref refs/heads/keeper {moved_keeper} {}\n",
                fixture.keeper_oid
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();

        let result = delete_if_unchanged(
            fixture.remote.to_str().unwrap(),
            "refs/heads/candidate",
            &fixture.candidate_oid,
            "refs/heads/keeper",
            &fixture.keeper_oid,
        );

        // Git omits the unchanged keeper update from the push transaction.
        // A receive hook can therefore move keeper while candidate deletion
        // proceeds. This test deliberately captures the guarantee boundary.
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fixture.remote_oid("refs/heads/candidate"), None);
        assert_eq!(
            fixture.remote_oid("refs/heads/keeper").as_deref(),
            Some(moved_keeper.as_str())
        );
    }
}
