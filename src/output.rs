//! Human-readable and JSON rendering for `dupdup check`, `dupdup analyze`,
//! and `dupdup mutate`.

use crate::redundancy::{RedundancyFinding, RedundancyKind};
use crate::static_lint::rules::{Severity, Violation};
use serde::Serialize;
use std::collections::HashMap;

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
            let more = if f.other_matches > 0 {
                format!(" (+{} more)", f.other_matches)
            } else {
                String::new()
            };
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
    }
}

#[derive(Serialize)]
struct JsonFinding<'a> {
    source: &'static str,
    kind: RedundancyKind,
    test_id: &'a str,
    related: &'a [String],
    other_matches: usize,
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
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&flat)?);
    Ok(())
}

#[derive(Serialize)]
struct KillMatrix<'a> {
    tests: &'a HashMap<String, Vec<String>>,
}

/// Prints a mutation collector's kill matrix in the
/// `{"tests": {"<test id>": ["<mutant id>", ...]}}` shape
/// `dynamic::killmatrix::load` reads back in.
///
/// # Errors
/// Returns an error if serialization fails (never expected in practice).
pub fn print_kill_matrix_json(kills: &HashMap<String, Vec<String>>) -> anyhow::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&KillMatrix { tests: kills })?
    );
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
                })
                .collect(),
            tests_analyzed: analyzed,
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
        });
        assert_eq!(r.flagged_tests(), 0);
    }
}
