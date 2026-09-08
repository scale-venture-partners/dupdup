use crate::redundancy::TestCoverage;
use fixedbitset::FixedBitSet;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::Path;

/// Shape of `coverage json --show-contexts`. Only the `contexts` map is
/// used — line -> the list of dynamic contexts (test ids) that executed it.
#[derive(Debug, Deserialize)]
struct CoverageReport {
    files: HashMap<String, FileReport>,
}

#[derive(Debug, Deserialize)]
struct FileReport {
    contexts: HashMap<String, Vec<String>>,
}

fn read_report(path: &Path) -> anyhow::Result<CoverageReport> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// A coverage run's per-test line coverage, ready for `redundancy::analyze`.
pub struct LoadedCoverage {
    /// One entry per test id found in the coverage-json contexts.
    pub tests: Vec<TestCoverage>,
}

/// A non-empty context is `<test id>|<phase>` (pytest-cov only records the
/// `run` phase); the empty string is the baseline context for code that ran
/// outside any test.
fn context_test_id(context: &str) -> Option<&str> {
    if context.is_empty() {
        return None;
    }
    context.split('|').next()
}

/// Loads a `coverage json --show-contexts` report from `path` and inverts
/// its line-to-contexts map into one bitset per test.
///
/// # Errors
/// Returns an error if `path` can't be read or isn't valid coverage-json.
pub fn load(path: &Path) -> anyhow::Result<LoadedCoverage> {
    let report = read_report(path)?;

    let mut requirement_index: HashMap<(String, String), usize> = HashMap::new();
    let mut per_test: HashMap<String, Vec<usize>> = HashMap::new();
    let mut next_index = 0usize;

    for (file, file_report) in &report.files {
        for (line, contexts) in &file_report.contexts {
            let key = (file.clone(), line.clone());
            let req_idx = *requirement_index.entry(key).or_insert_with(|| {
                let idx = next_index;
                next_index += 1;
                idx
            });
            for context in contexts {
                if let Some(test_id) = context_test_id(context) {
                    per_test
                        .entry(test_id.to_string())
                        .or_default()
                        .push(req_idx);
                }
            }
        }
    }

    let universe = next_index.max(1);
    let mut test_ids: Vec<String> = per_test.keys().cloned().collect();
    test_ids.sort();

    let tests = test_ids
        .into_iter()
        .map(|id| {
            let mut bits = FixedBitSet::with_capacity(universe);
            for &idx in &per_test[&id] {
                bits.set(idx, true);
            }
            TestCoverage { test_id: id, bits }
        })
        .collect();

    Ok(LoadedCoverage { tests })
}

/// Per-`(file, line)` coverage, keyed exactly as `coverage json` recorded
/// the file path — used by the mutation collector to select, for a mutant
/// on a given line, only the tests that could plausibly kill it, instead
/// of the whole suite.
pub struct LineCoverage {
    by_line: HashMap<(String, usize), Vec<String>>,
    files: HashSet<String>,
}

impl LineCoverage {
    /// Whether `file` appears in the coverage report at all. A mutation
    /// target that isn't found here is a path-spelling mismatch between
    /// `--source` and the coverage report, not "this file has no coverage"
    /// — see [`Self::known_files`] to help pin down the mismatch.
    #[must_use]
    pub fn has_file(&self, file: &str) -> bool {
        self.files.contains(file)
    }

    /// Every file path the coverage report has an entry for, sorted.
    #[must_use]
    pub fn known_files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = self.files.iter().map(String::as_str).collect();
        files.sort_unstable();
        files
    }

    /// Test ids that cover any line in `lines` of `file`, deduplicated.
    /// Empty means no test in this run executed any of those lines at all
    /// — a mutant restricted to this candidate set can never be
    /// attributed a kill, which callers must treat as "no coverage data",
    /// not as "every candidate test passed".
    #[must_use]
    pub fn tests_covering(&self, file: &str, lines: RangeInclusive<usize>) -> Vec<String> {
        let mut out: Vec<String> = lines
            .filter_map(|line| self.by_line.get(&(file.to_string(), line)))
            .flatten()
            .cloned()
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Loads a `coverage json --show-contexts` report from `path` into a
/// `(file, line) -> covering test ids` map.
///
/// # Errors
/// Returns an error if `path` can't be read or isn't valid coverage-json.
pub fn load_line_coverage(path: &Path) -> anyhow::Result<LineCoverage> {
    let report = read_report(path)?;
    let mut by_line: HashMap<(String, usize), Vec<String>> = HashMap::new();
    let mut files = HashSet::new();

    for (file, file_report) in &report.files {
        files.insert(file.clone());
        for (line, contexts) in &file_report.contexts {
            let Ok(line_no) = line.parse::<usize>() else {
                continue;
            };
            for context in contexts {
                if let Some(test_id) = context_test_id(context) {
                    by_line
                        .entry((file.clone(), line_no))
                        .or_default()
                        .push(test_id.to_string());
                }
            }
        }
    }

    Ok(LineCoverage { by_line, files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_report(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn identical_coverage_produces_identical_bitsets() {
        let f = write_report(
            r#"{
                "files": {
                    "pkg/mod.py": {
                        "contexts": {
                            "1": [""],
                            "2": ["test_mod.py::test_add|run", "test_mod.py::test_add_dup|run"],
                            "5": ["test_mod.py::test_sub|run"]
                        }
                    }
                }
            }"#,
        );
        let loaded = load(f.path()).unwrap();
        let by_id: HashMap<_, _> = loaded
            .tests
            .iter()
            .map(|t| (t.test_id.clone(), t.bits.clone()))
            .collect();
        assert_eq!(
            by_id["test_mod.py::test_add"],
            by_id["test_mod.py::test_add_dup"]
        );
        assert_ne!(
            by_id["test_mod.py::test_add"],
            by_id["test_mod.py::test_sub"]
        );
    }

    #[test]
    fn empty_context_is_not_a_test() {
        let f = write_report(r#"{"files": {"pkg/mod.py": {"contexts": {"1": [""]}}}}"#);
        let loaded = load(f.path()).unwrap();
        assert!(loaded.tests.is_empty());
    }

    #[test]
    fn line_coverage_finds_tests_covering_any_line_in_a_range() {
        let f = write_report(
            r#"{
                "files": {
                    "pkg/mod.py": {
                        "contexts": {
                            "1": ["test_mod.py::test_a|run"],
                            "2": ["test_mod.py::test_b|run"],
                            "3": ["test_mod.py::test_a|run"]
                        }
                    }
                }
            }"#,
        );
        let loaded = load_line_coverage(f.path()).unwrap();
        let mut covering = loaded.tests_covering("pkg/mod.py", 2..=3);
        covering.sort();
        assert_eq!(covering, vec!["test_mod.py::test_a", "test_mod.py::test_b"]);
    }

    #[test]
    fn line_coverage_is_empty_for_an_uncovered_line() {
        let f = write_report(
            r#"{"files": {"pkg/mod.py": {"contexts": {"1": ["test_mod.py::test_a|run"]}}}}"#,
        );
        let loaded = load_line_coverage(f.path()).unwrap();
        assert!(loaded.tests_covering("pkg/mod.py", 99..=99).is_empty());
    }

    #[test]
    fn line_coverage_reports_known_files_for_a_path_mismatch() {
        let f = write_report(
            r#"{"files": {"pkg/mod.py": {"contexts": {"1": ["test_mod.py::test_a|run"]}}}}"#,
        );
        let loaded = load_line_coverage(f.path()).unwrap();
        assert!(loaded.has_file("pkg/mod.py"));
        assert!(!loaded.has_file("pkg/other.py"));
        assert_eq!(loaded.known_files(), vec!["pkg/mod.py"]);
    }
}
