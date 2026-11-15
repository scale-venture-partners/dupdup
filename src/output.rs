//! Human-readable and JSON rendering for `dupdup check`, `dupdup analyze`,
//! and `dupdup mutate`.

use crate::redundancy::{RedundancyFinding, RedundancyKind};
use crate::static_lint::rules::{Severity, Violation};
use serde::Serialize;

/// Prints `violations` as one line each, then an error/warning summary.
pub fn print_check_human(violations: &[Violation]) {
    for v in violations {
        let sev = match v.severity {
            Severity::Error => "error",
            Severity::Warn => "warn ",
        };
        println!(
            "{}:{}  {} {}  {} [{}]",
            v.file, v.line, sev, v.rule, v.message, v.test_name
        );
    }
    let errors = violations
        .iter()
        .filter(|v| v.severity == Severity::Error)
        .count();
    let warnings = violations.len() - errors;
    println!();
    println!(
        "{} error(s), {} warning(s) across {} violation(s)",
        errors,
        warnings,
        violations.len()
    );
}

/// Prints `violations` as a JSON array.
///
/// # Errors
/// Returns an error if serialization fails (never expected in practice).
pub fn print_check_json(violations: &[Violation]) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(violations)?);
    Ok(())
}

fn kind_label(kind: RedundancyKind) -> &'static str {
    match kind {
        RedundancyKind::DuplicateCoverage => "DUPLICATE_COVERAGE",
        RedundancyKind::SubsumedCoverage => "SUBSUMED_COVERAGE",
        RedundancyKind::ContributesNothingUnique => "CONTRIBUTES_NOTHING_UNIQUE",
        RedundancyKind::CoversNothing => "COVERS_NOTHING",
        RedundancyKind::NoCoverage => "NO_COVERAGE",
        RedundancyKind::Removable => "REMOVABLE",
    }
}

/// One dynamic-tier source (`coverage` or `mutation`) and the findings its
/// own `redundancy::analyze` run produced. Kept separate at this layer
/// because the two sources' bitsets live in unrelated requirement spaces —
/// a coverage line-index and a mutant-id index mean different things at the
/// same bit position, so they're never merged into one `analyze` call.
pub struct AnalyzeReport {
    /// `"coverage"` or `"mutation"`.
    pub source: &'static str,
    /// Findings from running `redundancy::analyze` over this source alone.
    pub findings: Vec<RedundancyFinding>,
    /// How many tests the analysis ran over, for the flagged-ratio summary.
    pub tests_analyzed: usize,
    /// How much of the suite's time the flagged tests take, when a JUnit
    /// report gave times.
    pub runtime: Option<crate::redundancy::Runtime>,
}

/// A flagged fraction above this means the coverage signal isn't telling the
/// tests apart (typically shared setup), not that most of the suite is
/// redundant.
const SUSPICIOUS_FLAGGED_RATIO: f64 = 0.20;

fn is_relation(kind: RedundancyKind) -> bool {
    matches!(
        kind,
        RedundancyKind::DuplicateCoverage
            | RedundancyKind::SubsumedCoverage
            | RedundancyKind::ContributesNothingUnique
            | RedundancyKind::CoversNothing
    )
}

impl AnalyzeReport {
    /// "time: the flagged tests take X s of Y s (P%)", plus what the removal set
    /// would take, when the findings carry times.
    #[must_use]
    pub fn runtime_summary(&self) -> Option<String> {
        let rt = self.runtime?;
        if rt.total_seconds <= 0.0 {
            return None;
        }
        let mut line = format!(
            "{}: flagged tests take {:.1}s of {:.1}s ({:.0}%), an upper bound on the time acting on every finding would save",
            self.source,
            rt.flagged_seconds,
            rt.total_seconds,
            100.0 * rt.flagged_seconds / rt.total_seconds
        );
        let removable: f64 = self
            .findings
            .iter()
            .filter(|f| f.kind == RedundancyKind::Removable)
            .filter_map(|f| f.seconds)
            .sum();
        if removable > 0.0 {
            line.push_str(&format!("; the removal set takes {removable:.1}s"));
        }
        Some(line)
    }

    /// How many distinct tests carry a redundancy-relation finding.
    #[must_use]
    pub fn flagged_tests(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| is_relation(f.kind))
            .map(|f| f.test_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// `"<source>: N of M tests flagged (P%)"`, with a caution appended when
    /// the share is large enough that the list shouldn't be acted on as is.
    #[must_use]
    pub fn flagged_summary(&self) -> Option<String> {
        if self.tests_analyzed == 0 {
            return None;
        }
        let flagged = self.flagged_tests();
        #[allow(clippy::cast_precision_loss)]
        let ratio = flagged as f64 / self.tests_analyzed as f64;
        let mut line = format!(
            "{}: {} of {} tests flagged ({:.0}%)",
            self.source,
            flagged,
            self.tests_analyzed,
            ratio * 100.0
        );
        if ratio > SUSPICIOUS_FLAGGED_RATIO {
            line.push_str(
                " — a share this large usually means the signal isn't telling tests apart \
                 (shared setup, same code with different assertions); review before acting",
            );
        }
        Some(line)
    }
}

/// Prints each report's findings under a `-- source --` header, then a
/// total count.
pub fn print_analyze_human(reports: &[AnalyzeReport]) {
    let mut total = 0;
    for report in reports {
        println!("-- {} --", report.source);
        for f in &report.findings {
            let mut more = if f.other_matches > 0 {
                format!(" (+{} more)", f.other_matches)
            } else {
                String::new()
            };
            if let Some(n) = f.evaluated {
                more.push_str(&format!(" (0 of {n} mutants killed)"));
            }
            if let Some(s) = f.seconds {
                more.push_str(&format!(" ({s:.2}s)"));
            }
            if f.related.is_empty() {
                println!("{}  {}", kind_label(f.kind), f.test_id);
            } else {
                println!(
                    "{}  {}  (vs {}){more}",
                    kind_label(f.kind),
                    f.test_id,
                    f.related.join(", ")
                );
            }
        }
        total += report.findings.len();
    }
    println!();
    println!("{total} finding(s)");
    for report in reports {
        if let Some(line) = report.flagged_summary() {
            println!("{line}");
        }
        if let Some(line) = report.runtime_summary() {
            println!("{line}");
        }
    }
}

#[derive(Serialize)]
struct JsonFinding<'a> {
    source: &'static str,
    kind: RedundancyKind,
    test_id: &'a str,
    related: &'a [String],
    other_matches: usize,
    evaluated: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds: Option<f64>,
}

/// Prints every report's findings as one flat JSON array, each entry tagged
/// with its `source`.
///
/// # Errors
/// Returns an error if serialization fails (never expected in practice).
pub fn print_analyze_json(reports: &[AnalyzeReport]) -> anyhow::Result<()> {
    let flat: Vec<JsonFinding> = reports
        .iter()
        .flat_map(|r| {
            r.findings.iter().map(move |f| JsonFinding {
                source: r.source,
                kind: f.kind,
                test_id: &f.test_id,
                related: &f.related,
                other_matches: f.other_matches,
                evaluated: f.evaluated,
                seconds: f.seconds,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&flat)?);
    Ok(())
}

/// Prints a mutation collector's kill matrix in the shape
/// `dynamic::killmatrix::load` reads back in.
///
/// # Errors
/// Returns an error if serialization fails (never expected in practice).
pub fn print_kill_matrix_json(matrix: &crate::mutate::KillMatrix) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(matrix)?);
    Ok(())
}

/// Writes the kill matrix to `path`, replacing it atomically so a reader (or
/// an interrupted run) never sees a half-written file.
///
/// # Errors
/// Returns an error if the file can't be written.
pub fn write_kill_matrix_json(
    path: &std::path::Path,
    matrix: &crate::mutate::KillMatrix,
) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(matrix)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(flagged: usize, analyzed: usize) -> AnalyzeReport {
        AnalyzeReport {
            source: "coverage",
            findings: (0..flagged)
                .map(|i| RedundancyFinding {
                    kind: RedundancyKind::SubsumedCoverage,
                    test_id: format!("f.py::t{i}"),
                    related: vec![],
                    other_matches: 0,
                    evaluated: None,
                    seconds: None,
                })
                .collect(),
            tests_analyzed: analyzed,
            runtime: None,
        }
    }

    #[test]
    fn a_small_flagged_share_has_no_caution() {
        let s = report(1, 100).flagged_summary().unwrap();
        assert_eq!(s, "coverage: 1 of 100 tests flagged (1%)");
    }

    #[test]
    fn a_large_flagged_share_carries_a_caution() {
        let s = report(50, 100).flagged_summary().unwrap();
        assert!(s.contains("50 of 100 tests flagged (50%)"));
        assert!(s.contains("review before acting"));
    }

    #[test]
    fn no_tests_has_no_summary() {
        assert!(report(0, 0).flagged_summary().is_none());
    }

    #[test]
    fn no_coverage_and_removable_findings_are_not_counted_as_flagged() {
        let mut r = report(0, 10);
        r.findings.push(RedundancyFinding {
            kind: RedundancyKind::NoCoverage,
            test_id: "t::x".into(),
            related: vec![],
            other_matches: 0,
            evaluated: None,
            seconds: None,
        });
        assert_eq!(r.flagged_tests(), 0);
    }

    #[test]
    fn an_evaluated_count_is_shown_on_a_covers_nothing_finding() {
        let mut r = report(0, 1);
        r.findings.push(RedundancyFinding {
            kind: RedundancyKind::CoversNothing,
            test_id: "f.py::t".into(),
            related: vec![],
            other_matches: 0,
            evaluated: Some(7),
            seconds: None,
        });
        let json = {
            let flat = serde_json::to_string(
                &r.findings
                    .iter()
                    .map(|f| f.evaluated)
                    .collect::<Vec<Option<usize>>>(),
            )
            .unwrap();
            flat
        };
        assert_eq!(json, "[7]");
    }

    #[test]
    fn a_written_kill_matrix_loads_back_with_its_scope() {
        let mut m = crate::mutate::KillMatrix::default();
        m.tests.insert("t.py::a".into(), vec!["m1".into()]);
        m.tests.insert("t.py::b".into(), vec![]);
        m.evaluated.insert("t.py::b".into(), 2);
        m.mutated.insert("pkg/x.py".into(), 3);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kills.json");
        write_kill_matrix_json(&path, &m).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        let loaded = crate::dynamic::killmatrix::load(&path).unwrap();
        assert_eq!(loaded.tests.len(), 2);
        assert_eq!(loaded.evaluated["t.py::b"], 2);
        assert_eq!(loaded.mutated["pkg/x.py"], 3);
    }

    #[test]
    fn an_older_matrix_without_scope_fields_still_resumes() {
        let m: crate::mutate::KillMatrix =
            serde_json::from_str(r#"{"tests": {"t.py::a": ["m1"]}}"#).unwrap();
        assert!(m.mutated.is_empty() && m.evaluated.is_empty());
    }
}
