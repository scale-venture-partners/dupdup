use crate::redundancy::TestCoverage;
use fixedbitset::FixedBitSet;
use serde::Deserialize;
use std::collections::HashMap;
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

pub struct LoadedCoverage {
    pub tests: Vec<TestCoverage>,
}

impl LoadedCoverage {
    pub fn test_ids(&self) -> impl Iterator<Item = &str> {
        self.tests.iter().map(|t| t.test_id.as_str())
    }
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

pub fn load(path: &Path) -> anyhow::Result<LoadedCoverage> {
    let text = std::fs::read_to_string(path)?;
    let report: CoverageReport = serde_json::from_str(&text)?;

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
                    per_test.entry(test_id.to_string()).or_default().push(req_idx);
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
        assert_ne!(by_id["test_mod.py::test_add"], by_id["test_mod.py::test_sub"]);
    }

    #[test]
    fn empty_context_is_not_a_test() {
        let f = write_report(
            r#"{"files": {"pkg/mod.py": {"contexts": {"1": [""]}}}}"#,
        );
        let loaded = load(f.path()).unwrap();
        assert!(loaded.tests.is_empty());
    }
}
