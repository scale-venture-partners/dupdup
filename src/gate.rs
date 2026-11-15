//! A diff-scoped check for a PR's new tests: does each added or edited test
//! catch anything the suite does not already catch?
//!
//! The cost scales with the diff, not the suite: only the touched tests are
//! run under coverage, only the functions they cover are mutated, and each
//! mutant is run against the touched tests plus a capped number of existing
//! tests that cover the same lines (from a coverage report of the default
//! branch, when one is supplied). Without that report a touched test is
//! judged against the mutants alone, which can say it kills nothing but not
//! that it duplicates another test.

use crate::changed::{Status, TouchedTest};
use crate::dynamic::coverage::{load_line_coverage, LineCoverage};
use crate::mutate::{self, KillMatrix, RunConfig};
use crate::redundancy::base_test_name;
use anyhow::Context;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What the mutants say about one touched test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// It kills at least one mutant no existing test kills.
    Unique {
        /// The mutants only touched tests killed.
        mutants: Vec<String>,
    },
    /// Everything it kills, an existing test kills too.
    Overlap {
        /// A few of the existing tests that kill the same mutants.
        with: Vec<String>,
    },
    /// It was run against mutants and killed none.
    KillsNothing {
        /// How many mutants it was run against.
        evaluated: usize,
    },
    /// No mutant was run against it: it covers no mutated function, or the
    /// time budget ran out first.
    NoEvidence,
}

/// One touched test and its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestVerdict {
    /// The test's node id (no parametrize ids).
    pub node_id: String,
    /// Whether the PR added it or edited it.
    pub status: Status,
    /// What the mutants say about it.
    #[serde(flatten)]
    pub verdict: Verdict,
}

/// Classifies each touched test from a kill matrix.
#[must_use]
pub fn classify(touched: &[TouchedTest], matrix: &KillMatrix) -> Vec<TestVerdict> {
    let touched_ids: HashSet<&str> = touched.iter().map(|t| t.node_id.as_str()).collect();
    let is_touched = |id: &str| touched_ids.contains(base_test_name(id));

    // Who killed each mutant, among the tests that are not touched.
    let mut existing_killers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (test, kills) in &matrix.tests {
        if is_touched(test) {
            continue;
        }
        for m in kills {
            existing_killers.entry(m).or_default().push(test);
        }
    }

    touched
        .iter()
        .map(|t| {
            let cases: Vec<&String> = matrix
                .tests
                .keys()
                .filter(|id| base_test_name(id) == t.node_id)
                .collect();
            let mut killed: Vec<&str> = cases
                .iter()
                .flat_map(|id| matrix.tests[*id].iter().map(String::as_str))
                .collect();
            killed.sort_unstable();
            killed.dedup();
            let evaluated: usize = cases
                .iter()
                .map(|id| matrix.evaluated.get(*id).copied().unwrap_or(0))
                .sum();

            let unique: Vec<String> = killed
                .iter()
                .filter(|m| !existing_killers.contains_key(*m))
                .map(|m| (*m).to_string())
                .collect();
            let verdict = if !unique.is_empty() {
                Verdict::Unique { mutants: unique }
            } else if !killed.is_empty() {
                let mut with: Vec<String> = killed
                    .iter()
                    .filter_map(|m| existing_killers.get(m).and_then(|v| v.first()))
                    .map(|s| (*s).to_string())
                    .collect();
                with.sort();
                with.dedup();
                with.truncate(3);
                Verdict::Overlap { with }
            } else if evaluated > 0 {
                Verdict::KillsNothing { evaluated }
            } else {
                Verdict::NoEvidence
            };
            TestVerdict {
                node_id: t.node_id.clone(),
                status: t.status,
                verdict,
            }
        })
        .collect()
}

/// How to run the gate.
pub struct GateOptions<'a> {
    /// The repository root.
    pub root: &'a Path,
    /// The pytest command, starting with a Python interpreter
    /// (e.g. `.venv/bin/python -m pytest ...`).
    pub pytest_cmd: Vec<String>,
    /// Packages coverage measures (`--cov=<pkg>`).
    pub cov_sources: Vec<String>,
    /// Per-test coverage of the whole suite on the default branch, when
    /// available (`coverage json --show-contexts`).
    pub baseline_coverage: Option<&'a Path>,
    /// Stop starting new mutants after this long.
    pub budget: Duration,
    /// The most existing tests per covered line to run beside the touched ones.
    pub max_existing: usize,
    /// Per-invocation pytest timeout.
    pub timeout: Duration,
    /// Fork this many workers per pytest process (0: one process per mutant).
    pub workers: usize,
}

/// A scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!("dupdup-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_ok(cmd: &mut Command, what: &str) -> anyhow::Result<()> {
    let status = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("failed to run {what}"))?;
    anyhow::ensure!(status.success(), "{what} failed (exit {status})");
    Ok(())
}

/// Runs the gate over `touched`.
///
/// # Errors
/// Returns an error if the coverage run or the mutation run can't be
/// performed.
pub fn run(opts: &GateOptions, touched: &[TouchedTest]) -> anyhow::Result<Vec<TestVerdict>> {
    if touched.is_empty() {
        return Ok(Vec::new());
    }
    let started = Instant::now();
    let python = opts.pytest_cmd.first().context("--pytest-cmd is empty")?;
    anyhow::ensure!(
        opts.pytest_cmd.get(1).map(String::as_str) == Some("-m"),
        "--pytest-cmd must start with a Python interpreter, e.g. `.venv/bin/python -m pytest`"
    );

    // 1. Coverage of just the touched tests.
    let scratch = Scratch::new()?;
    let data_file = scratch.0.join(".coverage");
    let json_file = scratch.0.join("touched.json");
    let ids: Vec<&str> = touched.iter().map(|t| t.node_id.as_str()).collect();
    let mut cov_run = Command::new(python);
    cov_run
        .args(&opts.pytest_cmd[1..])
        .args(opts.cov_sources.iter().map(|s| format!("--cov={s}")))
        .args(["--cov-context=test", "--cov-report="])
        .args(&ids)
        .env("COVERAGE_FILE", &data_file)
        .env("COVERAGE_CORE", "ctrace")
        .current_dir(opts.root);
    // A touched test may itself fail; the coverage it recorded is still used.
    let _ = cov_run
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to run the touched tests under coverage")?;
    run_ok(
        Command::new(python)
            .args(["-m", "coverage", "json", "--show-contexts", "-o"])
            .arg(&json_file)
            .env("COVERAGE_FILE", &data_file)
            .current_dir(opts.root),
        "`coverage json` over the touched tests",
    )?;
    let touched_cov = load_line_coverage(&json_file)?;
    eprintln!(
        "touched tests run under coverage in {:.1}s",
        started.elapsed().as_secs_f32()
    );

    // 2. What to mutate against: touched tests plus a few existing ones.
    let existing = opts.baseline_coverage.map(load_line_coverage).transpose()?;
    let touched_set: HashSet<&str> = ids.iter().copied().collect();
    let is_touched = |id: &str| touched_set.contains(base_test_name(id));
    let gate_cov = LineCoverage::for_gate(
        &touched_cov,
        existing.as_ref(),
        &is_touched,
        opts.max_existing,
    );

    // 3. Mutate the covered functions (not the tests) within the budget.
    let sources: Vec<PathBuf> = gate_cov
        .covered_files()
        .into_iter()
        .filter(|f| f.ends_with(".py") && !is_test_path(f) && opts.root.join(f).exists())
        .map(PathBuf::from)
        .collect();
    // Function-level mutants show a test depends on a function; statement-level
    // ones show what it checks inside it. Both, within one shared budget.
    let mut matrix = KillMatrix::default();
    let touched_for_cap: HashSet<String> = ids.iter().map(|s| (*s).to_string()).collect();
    let max_existing = opts.max_existing;
    let narrow: mutate::NarrowCandidates = std::sync::Arc::new(move |all: Vec<String>| {
        let (mut mine, others): (Vec<String>, Vec<String>) = all
            .into_iter()
            .partition(|id| touched_for_cap.contains(base_test_name(id)));
        mine.extend(others.into_iter().take(max_existing));
        mine
    });
    for extreme in [true, false] {
        let cfg = RunConfig {
            pytest_cmd: opts.pytest_cmd.clone(),
            timeout: opts.timeout,
            extreme,
            max_drilldown: 100,
            env: Vec::new(),
            cwd: Some(opts.root.to_path_buf()),
            deadline: Some(started + opts.budget),
            narrow_candidates: Some(narrow.clone()),
            workers: opts.workers,
        };
        let part = mutate::collect(opts.root, &sources, &gate_cov, &cfg, None, &mut |_| Ok(()))?;
        merge(&mut matrix, part);
    }
    Ok(classify(touched, &matrix))
}

/// Adds `part`'s kills and evaluated counts to `into`.
fn merge(into: &mut KillMatrix, part: KillMatrix) {
    for (test, kills) in part.tests {
        into.tests.entry(test).or_default().extend(kills);
    }
    for (test, n) in part.evaluated {
        *into.evaluated.entry(test).or_default() += n;
    }
    into.mutated.extend(part.mutated);
}

fn is_test_path(path: &str) -> bool {
    path.split('/').any(|p| p == "tests" || p == "test")
        || path.rsplit('/').next().is_some_and(|n| {
            n.starts_with("test_") || n.ends_with("_test.py") || n == "conftest.py"
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touched(ids: &[&str]) -> Vec<TouchedTest> {
        ids.iter()
            .map(|id| TouchedTest {
                node_id: (*id).to_string(),
                status: Status::New,
            })
            .collect()
    }

    fn matrix(rows: &[(&str, &[&str], usize)]) -> KillMatrix {
        let mut m = KillMatrix::default();
        for (test, kills, evaluated) in rows {
            m.tests.insert(
                (*test).to_string(),
                kills.iter().map(|k| (*k).to_string()).collect(),
            );
            m.evaluated.insert((*test).to_string(), *evaluated);
        }
        m
    }

    fn verdict_of(v: &[TestVerdict], id: &str) -> Verdict {
        v.iter().find(|t| t.node_id == id).unwrap().verdict.clone()
    }

    #[test]
    fn a_test_that_kills_a_mutant_nobody_else_kills_is_unique() {
        let m = matrix(&[("t.py::new", &["m1", "m2"], 3), ("t.py::old", &["m1"], 3)]);
        let v = classify(&touched(&["t.py::new"]), &m);
        assert_eq!(
            verdict_of(&v, "t.py::new"),
            Verdict::Unique {
                mutants: vec!["m2".into()]
            }
        );
    }

    #[test]
    fn a_test_that_kills_only_what_an_existing_test_kills_overlaps_it() {
        let m = matrix(&[("t.py::new", &["m1"], 2), ("t.py::old", &["m1"], 2)]);
        let v = classify(&touched(&["t.py::new"]), &m);
        assert_eq!(
            verdict_of(&v, "t.py::new"),
            Verdict::Overlap {
                with: vec!["t.py::old".into()]
            }
        );
    }

    #[test]
    fn a_test_that_ran_against_mutants_and_killed_none_kills_nothing() {
        let m = matrix(&[("t.py::new", &[], 4), ("t.py::old", &["m1"], 4)]);
        let v = classify(&touched(&["t.py::new"]), &m);
        assert_eq!(
            verdict_of(&v, "t.py::new"),
            Verdict::KillsNothing { evaluated: 4 }
        );
    }

    #[test]
    fn a_test_with_no_mutant_run_against_it_has_no_evidence() {
        let m = matrix(&[("t.py::old", &["m1"], 1)]);
        let v = classify(&touched(&["t.py::new"]), &m);
        assert_eq!(verdict_of(&v, "t.py::new"), Verdict::NoEvidence);
    }

    #[test]
    fn parametrize_cases_of_a_touched_test_are_judged_together() {
        let m = matrix(&[
            ("t.py::new[a]", &[], 2),
            ("t.py::new[b]", &["m9"], 2),
            ("t.py::old", &["m1"], 2),
        ]);
        let v = classify(&touched(&["t.py::new"]), &m);
        assert_eq!(
            verdict_of(&v, "t.py::new"),
            Verdict::Unique {
                mutants: vec!["m9".into()]
            }
        );
    }

    #[test]
    fn two_touched_tests_do_not_count_as_each_others_existing_killers() {
        let m = matrix(&[("t.py::a", &["m1"], 1), ("t.py::b", &["m1"], 1)]);
        let v = classify(&touched(&["t.py::a", "t.py::b"]), &m);
        assert!(matches!(verdict_of(&v, "t.py::a"), Verdict::Unique { .. }));
        assert!(matches!(verdict_of(&v, "t.py::b"), Verdict::Unique { .. }));
    }

    #[test]
    fn merging_two_runs_adds_kills_and_evaluated_counts() {
        let mut a = matrix(&[("t.py::x", &["m1"], 2)]);
        merge(
            &mut a,
            matrix(&[("t.py::x", &["m2"], 3), ("t.py::y", &[], 1)]),
        );
        assert_eq!(a.tests["t.py::x"], vec!["m1", "m2"]);
        assert_eq!(a.evaluated["t.py::x"], 5);
        assert!(a.tests["t.py::y"].is_empty());
    }

    #[test]
    fn test_paths_are_not_mutated() {
        assert!(is_test_path("tests/test_a.py"));
        assert!(is_test_path("pkg/tests/helpers.py"));
        assert!(is_test_path("pkg/conftest.py"));
        assert!(is_test_path("pkg/foo_test.py"));
        assert!(!is_test_path("pkg/service.py"));
    }
}
