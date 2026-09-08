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
}

/// Prints each report's findings under a `-- source --` header, then a
/// total count.
pub fn print_analyze_human(reports: &[AnalyzeReport]) {
    let mut total = 0;
    for report in reports {
        println!("-- {} --", report.source);
        for f in &report.findings {
            if f.related.is_empty() {
                println!("{}  {}", kind_label(f.kind), f.test_id);
            } else {
                println!(
                    "{}  {}  (vs {})",
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
}

#[derive(Serialize)]
struct JsonFinding<'a> {
    source: &'static str,
    kind: RedundancyKind,
    test_id: &'a str,
    related: &'a [String],
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
