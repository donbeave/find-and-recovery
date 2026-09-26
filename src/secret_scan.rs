use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FindingException {
    #[serde(rename = "rule_id")]
    pub rule_id: String,
    pub start_line: u64,
    pub end_line: u64,
    pub blob_sha256: String,
}

#[derive(Debug)]
pub struct Finding {
    pub rule_id: String,
    pub file: String,
    pub commit: String,
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReportFinding {
    #[serde(rename = "RuleID")]
    rule_id: String,
    file: String,
    #[serde(default)]
    commit: String,
    start_line: u64,
    end_line: u64,
}

pub fn parse_report(bytes: &[u8]) -> Result<Vec<Finding>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let report: Vec<ReportFinding> = serde_json::from_slice(bytes)
        .map_err(|_| "secret scanner report was malformed; upload blocked".to_owned())?;
    report
        .into_iter()
        .map(|row| {
            if row.rule_id.is_empty()
                || row.file.is_empty()
                || row.start_line == 0
                || row.end_line < row.start_line
            {
                return Err("secret scanner report was incomplete; upload blocked".into());
            }
            Ok(Finding {
                rule_id: row.rule_id,
                file: row.file,
                commit: row.commit,
                start_line: row.start_line,
                end_line: row.end_line,
            })
        })
        .collect()
}

/// Apply exact-byte exceptions. Each baseline row can authorize one finding;
/// repeated rows are required to authorize repeated identical detections.
pub fn evaluate_report<F>(
    status_code: Option<i32>,
    report: &[u8],
    exceptions: &[FindingException],
    mut read_blob: F,
) -> Result<(), String>
where
    F: FnMut(&Finding) -> Result<Vec<u8>, String>,
{
    let findings = parse_report(report)?;
    match status_code {
        Some(0) if findings.is_empty() => return Ok(()),
        Some(1) if !findings.is_empty() => {}
        _ => {
            return Err(
                "secret scanner failed or returned an unexpected status; upload blocked".into(),
            );
        }
    }

    let mut available = BTreeMap::<(String, u64, u64, String), usize>::new();
    for exception in exceptions {
        if exception.rule_id.is_empty()
            || exception.start_line == 0
            || exception.end_line < exception.start_line
            || exception.blob_sha256.len() != 64
            || !exception.blob_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("secret finding exception baseline is invalid; upload blocked".into());
        }
        *available
            .entry((
                exception.rule_id.clone(),
                exception.start_line,
                exception.end_line,
                exception.blob_sha256.to_ascii_lowercase(),
            ))
            .or_default() += 1;
    }

    for finding in findings {
        let bytes = read_blob(&finding)?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let key = (
            finding.rule_id,
            finding.start_line,
            finding.end_line,
            digest,
        );
        let Some(count) = available.get_mut(&key) else {
            return Err("secret scan found an unapproved detection; upload blocked".into());
        };
        if *count == 0 {
            return Err("secret scan found a repeated unapproved detection; upload blocked".into());
        }
        *count -= 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT: &[u8] = b"checksum line\nknown reviewed match\n";
    const RULE: &str = "generic-api-key";

    fn exception() -> FindingException {
        FindingException {
            rule_id: RULE.into(),
            start_line: 2,
            end_line: 2,
            blob_sha256: format!("{:x}", Sha256::digest(CONTENT)),
        }
    }

    fn report(rule: &str, start: u64, end: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([{
            "RuleID": rule,
            "File": "relocated.txt",
            "Commit": "abc123",
            "StartLine": start,
            "EndLine": end,
            "Secret": "must never be retained",
            "Match": "must never be retained",
            "Fingerprint": "path:commit:rule:line"
        }]))
        .unwrap()
    }

    #[test]
    fn exact_content_exception_is_relocatable_and_redacted_from_model() {
        let report = report(RULE, 2, 2);
        let finding = parse_report(&report).unwrap().remove(0);
        assert_eq!(finding.file, "relocated.txt");
        assert_eq!(finding.commit, "abc123");
        assert!(!format!("{finding:?}").contains("must never"));
        evaluate_report(Some(1), &report, &[exception()], |_| Ok(CONTENT.to_vec())).unwrap();
    }

    #[test]
    fn changed_bytes_or_extra_finding_still_blocks() {
        let report = report(RULE, 2, 2);
        assert!(
            evaluate_report(Some(1), &report, &[exception()], |_| {
                Ok(b"checksum line\nchanged content\n".to_vec())
            })
            .is_err()
        );

        let mut two = serde_json::from_slice::<serde_json::Value>(&report).unwrap();
        let duplicate = two[0].clone();
        two.as_array_mut().unwrap().push(duplicate);
        let two = serde_json::to_vec(&two).unwrap();
        assert!(evaluate_report(Some(1), &two, &[exception()], |_| Ok(CONTENT.to_vec())).is_err());
    }

    #[test]
    fn malformed_report_and_scanner_error_block() {
        assert!(
            evaluate_report(Some(1), b"{not json", &[exception()], |_| Ok(
                CONTENT.to_vec()
            ))
            .is_err()
        );
        assert!(evaluate_report(Some(2), b"[]", &[exception()], |_| Ok(CONTENT.to_vec())).is_err());
        assert!(
            evaluate_report(Some(0), &report(RULE, 2, 2), &[exception()], |_| Ok(
                CONTENT.to_vec()
            ))
            .is_err()
        );
    }
}
