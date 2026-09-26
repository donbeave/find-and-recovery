//! Race-safe deletion of explicitly authorized remote branches.
//!
//! This module only deletes named `refs/heads/*` refs. It never deletes a
//! repository, tag, or local ref. The compatibility API remains fail-closed;
//! callers must provide a typed scope carrying the externally verified
//! repository and retention facts before the atomic primitive can run.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub const MAX_CANDIDATES_PER_TRANSACTION: usize = 32;
const MAX_CANDIDATES_PER_REQUEST: usize = 10_000;

/// Compatibility input for callers that only build a deletion preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub remote_ref: String,
    pub expected_oid: String,
    pub keeper_ref: String,
    pub keeper_oid: String,
}

/// A branch ref the caller proposes to delete at one exact object ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCandidate {
    pub reference: String,
    pub expected_oid: String,
}

/// Expected state for a protected branch ref. `Absent` lets the caller attest
/// that it reviewed a known-missing `main` or `master` ref as well.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefState {
    Present(String),
    Absent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefAssertion {
    pub reference: String,
    pub expected: RefState,
}

/// A durable ref and the candidate branch tips that an external verifier has
/// proven remain reachable from its asserted tip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionAnchor {
    pub reference: String,
    pub expected_oid: String,
    pub retained_candidate_oids: Vec<String>,
    /// Caller verified a server rule that prevents this ref from being moved
    /// or deleted after candidate deletion succeeds.
    pub retention_is_server_enforced: bool,
}

/// External facts the caller reviewed before it may attest a deletion scope.
/// These are explicit because the low-level Git primitive cannot inspect
/// GitHub's default branch, branch protection, rulesets, PRs, or keep policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewedDeletionFacts {
    pub repository_identity: String,
    pub actual_default_branch: RefAssertion,
    pub main_branch: RefAssertion,
    pub master_branch: RefAssertion,
    pub protections_and_rulesets_reviewed: bool,
    pub server_rejects_active_default_branch_deletion: bool,
    pub pull_request_heads_and_bases_reviewed: bool,
    pub keep_patterns_reviewed: bool,
    pub retention_anchors: Vec<RetentionAnchor>,
    /// Exact branch refs and object IDs reviewed as eligible for deletion.
    pub eligible_candidates: Vec<BranchCandidate>,
}

/// Opaque proof that a caller reviewed all external deletion-safety facts.
/// It is deliberately constructible only through the explicit attestation
/// function below.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDeletionScope {
    repository_identity: String,
    actual_default_branch: RefAssertion,
    main_branch: RefAssertion,
    master_branch: RefAssertion,
    retention_anchors: Vec<RetentionAnchor>,
    eligible_candidates: BTreeMap<String, String>,
}

impl VerifiedDeletionScope {
    /// Attest the external facts after verifying repository identity, actual
    /// default, `main`/`master`, branch protections and rulesets, PR head/base
    /// facts, keep patterns, and durable retention anchors.
    ///
    /// # Safety
    /// The primitive can check the supplied refs before and after every push,
    /// but cannot independently verify the GitHub API facts represented by
    /// the review flags or the reachability claims in each anchor. The caller
    /// must verify the active-default deletion guard and anchor-retention rule
    /// are server-enforced, check the external facts before setting the flags,
    /// verify every `retained_candidate_oids` entry, and verify each exact
    /// `eligible_candidates` ref/OID against protection, PR, and keep-policy
    /// facts before including it.
    pub unsafe fn attest(facts: ReviewedDeletionFacts) -> Result<Self, String> {
        validate_repository_identity(&facts.repository_identity)?;
        if !facts.protections_and_rulesets_reviewed
            || !facts.pull_request_heads_and_bases_reviewed
            || !facts.keep_patterns_reviewed
        {
            return Err(
                "deletion scope requires verified protections, PR facts, and keep patterns".into(),
            );
        }
        if !facts.server_rejects_active_default_branch_deletion {
            return Err(
                "deletion scope requires server-enforced protection for the active default branch"
                    .into(),
            );
        }

        validate_ref_assertion(&facts.actual_default_branch, true)?;
        if !matches!(&facts.actual_default_branch.expected, RefState::Present(_)) {
            return Err("the actual default branch must be asserted present".into());
        }
        if facts.main_branch.reference != "refs/heads/main"
            || facts.master_branch.reference != "refs/heads/master"
        {
            return Err("main/master assertions must name their exact branch refs".into());
        }
        validate_ref_assertion(&facts.main_branch, false)?;
        validate_ref_assertion(&facts.master_branch, false)?;

        if facts.retention_anchors.is_empty() {
            return Err("deletion scope requires at least one durable retention anchor".into());
        }
        let mut anchor_refs = BTreeSet::new();
        for anchor in &facts.retention_anchors {
            validate_branch_ref(&anchor.reference)?;
            validate_oid(&anchor.expected_oid)?;
            if !anchor.retention_is_server_enforced {
                return Err(format!(
                    "retention anchor {} lacks server-enforced durability",
                    anchor.reference
                ));
            }
            if !anchor_refs.insert(&anchor.reference) {
                return Err(format!(
                    "duplicate retention anchor ref: {}",
                    anchor.reference
                ));
            }
            if anchor.retained_candidate_oids.is_empty() {
                return Err(format!(
                    "retention anchor {} protects no candidate object IDs",
                    anchor.reference
                ));
            }
            let mut retained = BTreeSet::new();
            for oid in &anchor.retained_candidate_oids {
                validate_oid(oid)?;
                if !retained.insert(oid) {
                    return Err(format!(
                        "duplicate retained object ID on anchor {}: {oid}",
                        anchor.reference
                    ));
                }
            }
        }

        let mut scope = Self {
            repository_identity: facts.repository_identity,
            actual_default_branch: facts.actual_default_branch,
            main_branch: facts.main_branch,
            master_branch: facts.master_branch,
            retention_anchors: facts.retention_anchors,
            eligible_candidates: BTreeMap::new(),
        };
        let watched = scope.watched_refs()?;
        scope.eligible_candidates = validate_candidate_authorizations(
            &facts.eligible_candidates,
            &watched,
            &scope.retention_anchors,
        )?;
        Ok(scope)
    }

    fn watched_refs(&self) -> Result<BTreeMap<String, RefState>, String> {
        let mut watched = BTreeMap::new();
        for assertion in [
            &self.actual_default_branch,
            &self.main_branch,
            &self.master_branch,
        ] {
            insert_assertion(&mut watched, assertion)?;
        }
        for anchor in &self.retention_anchors {
            insert_assertion(
                &mut watched,
                &RefAssertion {
                    reference: anchor.reference.clone(),
                    expected: RefState::Present(anchor.expected_oid.clone()),
                },
            )?;
        }
        Ok(watched)
    }
}

/// A destination whose GitHub HTTPS URL and repository identity were parsed
/// before deletion was authorized.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedDestination {
    locator: String,
    repository_identity: String,
}

impl ValidatedDestination {
    /// Parse an HTTPS GitHub clone URL. Query strings, fragments, credentials,
    /// ports, path escapes, and non-GitHub hosts are rejected.
    pub fn github_https(target: &str) -> Result<Self, String> {
        let path = target
            .strip_prefix("https://")
            .ok_or("branch deletion supports validated GitHub HTTPS destinations only")?;
        let (host, path) = path
            .split_once('/')
            .ok_or("GitHub clone URL is missing owner/repository")?;
        if !host.eq_ignore_ascii_case("github.com") {
            return Err("branch deletion supports github.com only".into());
        }
        if path.contains(['?', '#', '@', '%', ':']) {
            return Err("GitHub clone URL has unsupported path syntax".into());
        }
        let path = path.strip_suffix('/').unwrap_or(path);
        let path = path.strip_suffix(".git").unwrap_or(path);
        validate_repository_identity(path)?;
        Ok(Self {
            locator: target.to_owned(),
            repository_identity: path.to_owned(),
        })
    }

    /// Local bare-remote constructor for disposable regression fixtures only.
    #[cfg(test)]
    pub fn local_bare_fixture(remote: &Path, repository_identity: &str) -> Result<Self, String> {
        validate_repository_identity(repository_identity)?;
        let canonical = remote
            .canonicalize()
            .map_err(|error| format!("local bare fixture is inaccessible: {error}"))?;
        if !canonical.join("HEAD").is_file()
            || !canonical.join("objects").is_dir()
            || !canonical.join("refs").is_dir()
        {
            return Err("local fixture destination is not a bare Git repository".into());
        }
        Ok(Self {
            locator: canonical.to_string_lossy().into_owned(),
            repository_identity: repository_identity.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionDisposition {
    Acknowledged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionReport {
    pub references: Vec<String>,
    pub disposition: TransactionDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionReport {
    /// Refs absent in an authoritative read after their atomic transaction.
    pub deleted_refs: Vec<String>,
    pub transactions: Vec<TransactionReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionError {
    /// Earlier complete transactions whose deletions were authoritatively
    /// confirmed. The current transaction is never listed here if ambiguous.
    pub confirmed_deleted: Vec<String>,
    pub outcome_ambiguous: bool,
    pub message: String,
}

impl fmt::Display for DeletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.confirmed_deleted.is_empty() {
            write!(
                f,
                "{} earlier branch deletion(s) were confirmed; ",
                self.confirmed_deleted.len()
            )?;
        }
        if self.outcome_ambiguous {
            write!(f, "transaction outcome is ambiguous: {}", self.message)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for DeletionError {}

/// Delete exact branch refs in bounded atomic transactions.
///
/// Every candidate and every default/main/master/retention assertion is read
/// from the remote before and after each transaction. Each deletion uses an
/// exact `--force-with-lease=<ref>:<expected-oid>` plus a deletion refspec.
/// There is no non-atomic fallback. The function only returns a deletion
/// report after authoritative reads prove all requested refs absent and all
/// protected refs unchanged.
pub fn delete_branches_if_unchanged(
    destination: &ValidatedDestination,
    scope: &VerifiedDeletionScope,
    candidates: &[BranchCandidate],
) -> Result<DeletionReport, DeletionError> {
    validate_request(destination, scope, candidates).map_err(|message| DeletionError {
        confirmed_deleted: Vec::new(),
        outcome_ambiguous: false,
        message,
    })?;
    if candidates.is_empty() {
        return Ok(DeletionReport {
            deleted_refs: Vec::new(),
            transactions: Vec::new(),
        });
    }

    let workspace = GitWorkspace::new().map_err(|message| DeletionError {
        confirmed_deleted: Vec::new(),
        outcome_ambiguous: false,
        message,
    })?;
    let mut watched = scope.watched_refs().map_err(|message| DeletionError {
        confirmed_deleted: Vec::new(),
        outcome_ambiguous: false,
        message,
    })?;
    let mut report = DeletionReport {
        deleted_refs: Vec::with_capacity(candidates.len()),
        transactions: Vec::new(),
    };

    for batch in candidates.chunks(MAX_CANDIDATES_PER_TRANSACTION) {
        let before =
            read_remote_refs(&workspace, destination).map_err(|message| DeletionError {
                confirmed_deleted: report.deleted_refs.clone(),
                outcome_ambiguous: false,
                message: format!(
                    "could not read authoritative remote refs before transaction: {message}"
                ),
            })?;
        validate_remote_state(&before, scope, &watched, batch).map_err(|message| {
            DeletionError {
                confirmed_deleted: report.deleted_refs.clone(),
                outcome_ambiguous: false,
                message,
            }
        })?;

        let push_result = push_delete_batch(&workspace, destination, batch);
        let after = match read_remote_refs(&workspace, destination) {
            Ok(after) => after,
            Err(read_error) => {
                let message = match &push_result {
                    Ok(()) => format!(
                        "atomic push returned success but its remote result could not be verified: {read_error}"
                    ),
                    Err(push_error) => format!(
                        "{push_error}; authoritative post-transaction read failed: {read_error}"
                    ),
                };
                return Err(DeletionError {
                    confirmed_deleted: report.deleted_refs,
                    outcome_ambiguous: true,
                    message,
                });
            }
        };

        let anchors_unchanged = default_head_matches(&after, &scope.actual_default_branch)
            && watched
                .iter()
                .all(|(reference, expected)| ref_state_matches(&after, reference, expected));
        let all_absent = batch
            .iter()
            .all(|candidate| !after.branches.contains_key(&candidate.reference));
        if all_absent && anchors_unchanged {
            if let Err(push_error) = push_result {
                return Err(DeletionError {
                    confirmed_deleted: report.deleted_refs,
                    outcome_ambiguous: true,
                    message: format!(
                        "candidate refs are absent after a failed atomic push; cannot attribute deletion to this transaction: {push_error}"
                    ),
                });
            }
            let references = batch
                .iter()
                .map(|candidate| candidate.reference.clone())
                .collect::<Vec<_>>();
            report.deleted_refs.extend(references.iter().cloned());
            for reference in &references {
                watched.insert(reference.clone(), RefState::Absent);
            }
            report.transactions.push(TransactionReport {
                references,
                disposition: TransactionDisposition::Acknowledged,
            });
            continue;
        }

        let all_candidates_unchanged = batch.iter().all(|candidate| {
            after.branches.get(&candidate.reference) == Some(&candidate.expected_oid)
        });
        if all_candidates_unchanged && anchors_unchanged && push_result.is_err() {
            let push_error = push_result.err().unwrap_or_default();
            return Err(DeletionError {
                confirmed_deleted: report.deleted_refs,
                outcome_ambiguous: false,
                message: format!("atomic branch deletion was not applied: {push_error}"),
            });
        }

        let push_detail = push_result
            .err()
            .unwrap_or_else(|| "Git acknowledged the push".into());
        return Err(DeletionError {
            confirmed_deleted: report.deleted_refs,
            outcome_ambiguous: true,
            message: format!(
                "remote refs or retention anchors differ from both the expected pre-state and the confirmed deletion state; {push_detail}"
            ),
        });
    }

    Ok(report)
}

/// Legacy entry point has no verified destination or retention proof, so it
/// always fails before running Git.
pub fn delete_candidates_if_unchanged(
    _remote: &str,
    _candidates: &[Candidate],
) -> Result<(), String> {
    Err("legacy remote deletion API is disabled by policy; a verified safety and retention scope is required".into())
}

fn validate_request(
    destination: &ValidatedDestination,
    scope: &VerifiedDeletionScope,
    candidates: &[BranchCandidate],
) -> Result<(), String> {
    if destination.repository_identity != scope.repository_identity {
        return Err("validated destination does not match the reviewed repository identity".into());
    }
    if candidates.len() > MAX_CANDIDATES_PER_REQUEST {
        return Err(format!(
            "deletion request exceeds the {}-branch limit",
            MAX_CANDIDATES_PER_REQUEST
        ));
    }
    if destination.locator.is_empty()
        || destination.locator.starts_with('-')
        || destination
            .locator
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err("invalid validated branch deletion destination".into());
    }
    let requested = validate_candidate_authorizations(
        candidates,
        &scope.watched_refs()?,
        &scope.retention_anchors,
    )?;
    if requested != scope.eligible_candidates {
        return Err(
            "candidate request does not exactly match the reviewed eligible refs and OIDs".into(),
        );
    }
    Ok(())
}

fn validate_candidate_authorizations(
    candidates: &[BranchCandidate],
    watched: &BTreeMap<String, RefState>,
    retention_anchors: &[RetentionAnchor],
) -> Result<BTreeMap<String, String>, String> {
    if candidates.len() > MAX_CANDIDATES_PER_REQUEST {
        return Err(format!(
            "eligible candidate set exceeds the {}-branch limit",
            MAX_CANDIDATES_PER_REQUEST
        ));
    }
    let mut authorized = BTreeMap::new();
    for candidate in candidates {
        validate_branch_ref(&candidate.reference)?;
        validate_oid(&candidate.expected_oid)?;
        if watched.contains_key(&candidate.reference) {
            return Err(format!(
                "candidate ref is also a protected or retention ref: {}",
                candidate.reference
            ));
        }
        if !retention_anchors.iter().any(|anchor| {
            anchor
                .retained_candidate_oids
                .iter()
                .any(|oid| oid == &candidate.expected_oid)
        }) {
            return Err(format!(
                "candidate object {} has no verified durable retention anchor",
                candidate.expected_oid
            ));
        }
        if authorized
            .insert(candidate.reference.clone(), candidate.expected_oid.clone())
            .is_some()
        {
            return Err(format!(
                "duplicate candidate branch ref: {}",
                candidate.reference
            ));
        }
    }
    Ok(authorized)
}

fn validate_remote_state(
    observed: &RemoteSnapshot,
    scope: &VerifiedDeletionScope,
    watched: &BTreeMap<String, RefState>,
    candidates: &[BranchCandidate],
) -> Result<(), String> {
    if !default_head_matches(observed, &scope.actual_default_branch) {
        return Err("remote's advertised default branch differs from the reviewed default".into());
    }
    for (reference, expected) in watched {
        if !ref_state_matches(observed, reference, expected) {
            return Err(format!(
                "protected or retention ref changed before transaction: {reference}"
            ));
        }
    }
    for candidate in candidates {
        match observed.branches.get(&candidate.reference) {
            Some(actual) if actual == &candidate.expected_oid => {}
            Some(actual) => {
                return Err(format!(
                    "candidate ref changed before transaction: {} is {actual}, expected {}",
                    candidate.reference, candidate.expected_oid
                ));
            }
            None => {
                return Err(format!(
                    "candidate ref is absent before transaction: {}",
                    candidate.reference
                ));
            }
        }
    }
    Ok(())
}

fn ref_state_matches(observed: &RemoteSnapshot, reference: &str, expected: &RefState) -> bool {
    match expected {
        RefState::Present(oid) => observed.branches.get(reference) == Some(oid),
        RefState::Absent => !observed.branches.contains_key(reference),
    }
}

fn default_head_matches(snapshot: &RemoteSnapshot, default: &RefAssertion) -> bool {
    matches!(&default.expected, RefState::Present(oid)
        if snapshot.head_target.as_deref() == Some(default.reference.as_str())
            && snapshot.branches.get(&default.reference) == Some(oid))
}

fn push_delete_batch(
    workspace: &GitWorkspace,
    destination: &ValidatedDestination,
    candidates: &[BranchCandidate],
) -> Result<(), String> {
    let mut command = workspace.git_command();
    command.args([
        "-c",
        "push.followTags=false",
        "push",
        "--atomic",
        "--porcelain",
        "--no-follow-tags",
        "--no-verify",
        "--no-recurse-submodules",
    ]);
    for candidate in candidates {
        command.arg(format!(
            "--force-with-lease={}:{}",
            candidate.reference, candidate.expected_oid
        ));
    }
    command.arg("--").arg(&destination.locator);
    for candidate in candidates {
        command.arg(format!(":{}", candidate.reference));
    }
    let output = command
        .output()
        .map_err(|error| format!("could not start atomic branch deletion push: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "atomic branch deletion push failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

struct RemoteSnapshot {
    branches: BTreeMap<String, String>,
    head_target: Option<String>,
}

fn read_remote_refs(
    workspace: &GitWorkspace,
    destination: &ValidatedDestination,
) -> Result<RemoteSnapshot, String> {
    let output = workspace
        .git_command()
        .args(["ls-remote", "--symref", "--"])
        .arg(&destination.locator)
        .args(["HEAD", "refs/heads/*"])
        .output()
        .map_err(|error| format!("could not start authoritative remote ref read: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "authoritative remote ref read returned invalid UTF-8".to_owned())?;
    let mut refs = BTreeMap::new();
    let mut head_oid = None;
    let mut head_target = None;
    for line in text.lines() {
        if let Some(row) = line.strip_prefix("ref: ") {
            let (target, symbolic_ref) = row
                .split_once('\t')
                .ok_or("authoritative remote ref read returned a malformed symref")?;
            if symbolic_ref == "HEAD" {
                validate_branch_ref(target).map_err(|_| {
                    "authoritative remote ref read returned an invalid default branch".to_owned()
                })?;
                if head_target.replace(target.to_owned()).is_some() {
                    return Err(
                        "authoritative remote ref read returned duplicate HEAD symrefs".into(),
                    );
                }
            }
            continue;
        }
        let (oid, reference) = line
            .split_once('\t')
            .ok_or("authoritative remote ref read returned a malformed row")?;
        validate_oid(oid).map_err(|_| {
            format!("authoritative remote ref read returned an invalid OID for {reference}")
        })?;
        if reference == "HEAD" {
            if head_oid.replace(oid.to_owned()).is_some() {
                return Err("authoritative remote ref read returned duplicate HEAD OIDs".into());
            }
            continue;
        }
        validate_branch_ref(reference).map_err(|_| {
            format!("authoritative remote ref read returned a non-branch ref: {reference}")
        })?;
        if let Some(previous) = refs.insert(reference.to_owned(), oid.to_owned())
            && previous != oid
        {
            return Err(format!(
                "authoritative remote ref read returned conflicting OIDs for {reference}"
            ));
        }
    }
    let head_target = head_target
        .ok_or("remote did not advertise a symbolic default branch; refusing deletion")?;
    let head_oid =
        head_oid.ok_or("remote did not advertise a default branch OID; refusing deletion")?;
    if refs.get(&head_target) != Some(&head_oid) {
        return Err("remote default branch OID differs within the authoritative snapshot".into());
    }
    Ok(RemoteSnapshot {
        branches: refs,
        head_target: Some(head_target),
    })
}

struct GitWorkspace {
    _temporary: tempfile::TempDir,
    repository: PathBuf,
    global_config: PathBuf,
    hooks: PathBuf,
}

impl GitWorkspace {
    fn new() -> Result<Self, String> {
        let temporary = tempfile::tempdir()
            .map_err(|error| format!("could not create isolated Git workspace: {error}"))?;
        let repository = temporary.path().join("scratch.git");
        let global_config = temporary.path().join("empty-global.gitconfig");
        let template = temporary.path().join("empty-template");
        let hooks = temporary.path().join("empty-hooks");
        fs::write(&global_config, b"")
            .map_err(|error| format!("could not create isolated Git config: {error}"))?;
        fs::create_dir(&template)
            .map_err(|error| format!("could not create empty Git template: {error}"))?;
        fs::create_dir(&hooks)
            .map_err(|error| format!("could not create empty Git hooks directory: {error}"))?;

        let mut command = isolated_git_command(&global_config);
        command
            .args(["init", "--bare", "--quiet", "--template"])
            .arg(&template)
            .arg(&repository);
        checked_output(command, "initialize isolated bare Git workspace")?;
        Ok(Self {
            _temporary: temporary,
            repository,
            global_config,
            hooks,
        })
    }

    fn git_command(&self) -> Command {
        let mut command = isolated_git_command(&self.global_config);
        command.arg(format!("--git-dir={}", self.repository.display()));
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", self.hooks.display()))
            .arg("-c")
            .arg("credential.helper=");
        command
    }
}

fn isolated_git_command(global_config: &Path) -> Command {
    let mut command = Command::new("git");
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_LFS_SKIP_PUSH", "1")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    command
}

fn checked_output(mut command: Command, action: &str) -> Result<Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not start {action}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not {action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn insert_assertion(
    watched: &mut BTreeMap<String, RefState>,
    assertion: &RefAssertion,
) -> Result<(), String> {
    validate_ref_assertion(assertion, false)?;
    if let Some(previous) = watched.insert(assertion.reference.clone(), assertion.expected.clone())
        && previous != assertion.expected
    {
        return Err(format!(
            "conflicting safety assertions for ref {}",
            assertion.reference
        ));
    }
    Ok(())
}

fn validate_ref_assertion(assertion: &RefAssertion, must_be_present: bool) -> Result<(), String> {
    validate_branch_ref(&assertion.reference)?;
    match &assertion.expected {
        RefState::Present(oid) => validate_oid(oid),
        RefState::Absent if must_be_present => Err(format!(
            "ref {} must be asserted present",
            assertion.reference
        )),
        RefState::Absent => Ok(()),
    }
}

fn validate_branch_ref(reference: &str) -> Result<(), String> {
    let name = reference
        .strip_prefix("refs/heads/")
        .ok_or_else(|| format!("only full refs/heads/* names are allowed: {reference}"))?;
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name
            .bytes()
            .any(|byte| byte <= b' ' || byte == 0x7f || b"~^:?*[\\".contains(&byte))
        || name.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.starts_with('.')
                || part.ends_with(".lock")
        })
    {
        return Err(format!("invalid branch ref: {reference}"));
    }
    Ok(())
}

fn validate_oid(oid: &str) -> Result<(), String> {
    if (oid.len() != 40 && oid.len() != 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || oid.bytes().all(|byte| byte == b'0')
    {
        return Err(format!(
            "expected a full lowercase 40- or 64-character OID: {oid}"
        ));
    }
    Ok(())
}

fn validate_repository_identity(identity: &str) -> Result<(), String> {
    let mut parts = identity.split('/');
    let (Some(owner), Some(repository), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("repository identity must be owner/repository".into());
    };
    if [owner, repository].iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || part.starts_with('-')
            || part.contains(['?', '#', ':', '@', '%', '\\'])
            || part
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    }) {
        return Err("repository identity has invalid owner/repository syntax".into());
    }
    Ok(())
}
