//! Batched Git LFS payload uploads.
//!
//! `git lfs push --object-id <remote> --stdin` accepts newline-delimited
//! SHA-256 object IDs. Validate all inputs and confirm every payload exists
//! before starting the upload, so a missing local object cannot be skipped.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Push all supplied LFS object IDs in one `git lfs push` invocation.
///
/// `remote` may be a configured remote name or an endpoint accepted by Git.
/// Errors intentionally omit child-process stderr because it may contain
/// filenames, endpoint credentials, or other sensitive details.
pub fn push_lfs_objects(repo: &Path, remote: &str, oids: &[String]) -> Result<(), String> {
    push_lfs_objects_with_git(OsStr::new("git"), repo, remote, oids)
}

fn push_lfs_objects_with_git(
    git_program: &OsStr,
    repo: &Path,
    remote: &str,
    oids: &[String],
) -> Result<(), String> {
    if oids.is_empty() {
        return Ok(());
    }
    if remote.trim().is_empty() {
        return Err("Git LFS remote is empty".into());
    }

    let mut normalized = Vec::with_capacity(oids.len());
    for oid in oids {
        if oid.len() != 64 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid Git LFS object ID; expected 64 hexadecimal characters".into());
        }
        normalized.push(oid.to_ascii_lowercase());
    }
    normalized.sort_unstable();
    if normalized.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("duplicate Git LFS object ID in upload batch".into());
    }

    let media_dir = local_media_dir(git_program, repo)?;
    let missing = normalized
        .iter()
        .filter(|oid| !payload_path(&media_dir, oid).is_file())
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "missing local Git LFS payload for object ID(s): {}",
            missing.join(", ")
        ));
    }

    let mut command = Command::new(git_program);
    command
        .arg("-C")
        .arg(repo)
        .args(["lfs", "push", "--object-id", remote, "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    scrub_git_environment(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| "could not start Git LFS object upload".to_owned())?;
    let write_result = {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or("Git LFS upload input unavailable")?;
        normalized
            .iter()
            .try_for_each(|oid| writeln!(stdin, "{oid}"))
    };
    drop(child.stdin.take());
    let status = child
        .wait()
        .map_err(|_| "could not wait for Git LFS object upload".to_owned())?;
    write_result.map_err(|_| "could not send Git LFS object IDs".to_owned())?;
    if !status.success() {
        return Err(format!("Git LFS object upload failed ({status})"));
    }
    Ok(())
}

fn local_media_dir(git_program: &OsStr, repo: &Path) -> Result<PathBuf, String> {
    let mut command = Command::new(git_program);
    command
        .arg("-C")
        .arg(repo)
        .args(["lfs", "env"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    scrub_git_environment(&mut command);
    let output = command
        .output()
        .map_err(|_| "could not locate local Git LFS storage".to_owned())?;
    checked(&output, "locate local Git LFS storage")?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "Git LFS storage information is not UTF-8".to_owned())?;
    text.lines()
        .find_map(|line| line.strip_prefix("LocalMediaDir="))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "Git LFS did not report local object storage".into())
}

fn payload_path(media_dir: &Path, oid: &str) -> PathBuf {
    media_dir.join(&oid[..2]).join(&oid[2..4]).join(oid)
}

fn checked(output: &Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{operation} failed ({})", output.status))
    }
}

fn scrub_git_environment(command: &mut Command) {
    // `-C` selects the source repository. Remove inherited overrides that can
    // redirect Git to another repository, index, or object database.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[cfg(unix)]
    struct FakeGit {
        _temp: TempDir,
        program: PathBuf,
        repo: PathBuf,
        media: PathBuf,
        calls: PathBuf,
        stdin_capture: PathBuf,
    }

    #[cfg(unix)]
    impl FakeGit {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let program = temp.path().join("git");
            let repo = temp.path().join("repo");
            let media = temp.path().join("media");
            let calls = temp.path().join("calls");
            let stdin_capture = temp.path().join("stdin");
            fs::create_dir_all(&repo).unwrap();
            fs::create_dir_all(&media).unwrap();
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in *' lfs env') printf 'LocalMediaDir=%s\\n' '{}' ;; *' lfs push '*) cat > '{}' ; case \"$*\" in *failremote*) echo SECRET_PAYLOAD >&2; exit 9 ;; esac ;; esac\n",
                calls.display(),
                media.display(),
                stdin_capture.display()
            );
            fs::write(&program, script).unwrap();
            let mut permissions = fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&program, permissions).unwrap();
            Self {
                _temp: temp,
                program,
                repo,
                media,
                calls,
                stdin_capture,
            }
        }

        fn add_payload(&self, oid: &str) {
            let oid = oid.to_ascii_lowercase();
            let path = payload_path(&self.media, &oid);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture payload").unwrap();
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(&self.calls)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[cfg(unix)]
    fn oid(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    #[cfg(unix)]
    #[test]
    fn batches_all_payloads_into_one_push() {
        let fake = FakeGit::new();
        let first = oid('a');
        let second = oid('b');
        fake.add_payload(&first);
        fake.add_payload(&second);

        push_lfs_objects_with_git(
            fake.program.as_os_str(),
            &fake.repo,
            "origin",
            &[second.clone(), first.clone()],
        )
        .unwrap();

        let calls = fake.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.contains("lfs push"))
                .count(),
            1
        );
        assert_eq!(
            calls.iter().filter(|call| call.contains("lfs env")).count(),
            1
        );
        assert_eq!(
            fs::read_to_string(fake.stdin_capture).unwrap(),
            format!("{first}\n{second}\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_malformed_oid_before_running_git() {
        let fake = FakeGit::new();
        let error = push_lfs_objects_with_git(
            fake.program.as_os_str(),
            &fake.repo,
            "origin",
            &["not-an-oid".into()],
        )
        .unwrap_err();
        assert!(error.contains("invalid Git LFS object ID"));
        assert!(fake.calls().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn empty_batch_is_a_noop() {
        let fake = FakeGit::new();
        push_lfs_objects_with_git(fake.program.as_os_str(), &fake.repo, "origin", &[]).unwrap();
        assert!(fake.calls().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn missing_payload_is_explicit_and_never_pushed() {
        let fake = FakeGit::new();
        let missing = oid('c');
        let error = push_lfs_objects_with_git(
            fake.program.as_os_str(),
            &fake.repo,
            "origin",
            std::slice::from_ref(&missing),
        )
        .unwrap_err();
        assert!(error.contains("missing local Git LFS payload"));
        assert!(error.contains(&missing));
        assert_eq!(fake.calls().len(), 1);
        assert!(fake.calls()[0].contains("lfs env"));
    }

    #[cfg(unix)]
    #[test]
    fn push_failure_does_not_leak_stderr() {
        let fake = FakeGit::new();
        let present = oid('d');
        fake.add_payload(&present);
        // The script's stderr is discarded; failure reports only status.
        let result = push_lfs_objects_with_git(
            fake.program.as_os_str(),
            &fake.repo,
            "failremote",
            std::slice::from_ref(&present),
        );
        let error = result.unwrap_err();
        assert!(error.contains("Git LFS object upload failed"));
        assert!(!error.contains("SECRET_PAYLOAD"));
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_ids_are_rejected() {
        let fake = FakeGit::new();
        let repeated = oid('e');
        let error = push_lfs_objects_with_git(
            fake.program.as_os_str(),
            &fake.repo,
            "origin",
            &[repeated.clone(), repeated],
        )
        .unwrap_err();
        assert!(error.contains("duplicate Git LFS object ID"));
        assert!(fake.calls().is_empty());
    }
}
