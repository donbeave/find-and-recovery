//! Remote branch deletion is disabled by repository policy.

/// A proposed remote branch deletion retained for callers that build previews.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub remote_ref: String,
    pub expected_oid: String,
    pub keeper_ref: String,
    pub keeper_oid: String,
}

/// Refuse every remote deletion. Recovery tooling must never delete remote refs.
pub fn delete_candidates_if_unchanged(
    _remote: &str,
    _candidates: &[Candidate],
) -> Result<(), String> {
    Err("remote branch deletion is disabled by policy".into())
}

/// Legacy single-candidate API; disabled for the same policy reason.
#[deprecated(note = "remote branch deletion is disabled by policy")]
pub fn delete_if_unchanged(
    _remote: &str,
    _candidate_ref: &str,
    _candidate_oid: &str,
    _keeper_ref: &str,
    _keeper_oid: &str,
) -> Result<(), String> {
    Err("remote branch deletion is disabled by policy".into())
}
