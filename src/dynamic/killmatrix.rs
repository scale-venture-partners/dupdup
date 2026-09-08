use crate::redundancy::TestCoverage;
use fixedbitset::FixedBitSet;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// `{"tests": {"<test id>": ["<mutant id>", ...]}}` — the kill-matrix input
/// contract for the mutation tier. No producer for this shape exists yet;
/// this loader lets the redundancy engine consume one once it does.
#[derive(Debug, Deserialize)]
struct KillMatrixReport {
    tests: HashMap<String, Vec<String>>,
}

/// A kill matrix's per-test killed-mutant coverage, ready for
/// `redundancy::analyze`.
pub struct LoadedKillMatrix {
    /// One entry per test id in the kill matrix.
    pub tests: Vec<TestCoverage>,
}

/// Loads a `{"tests": {"<test id>": ["<mutant id>", ...]}}` kill-matrix
/// report from `path` into one bitset per test.
///
/// # Errors
/// Returns an error if `path` can't be read or doesn't match that schema.
pub fn load(path: &Path) -> anyhow::Result<LoadedKillMatrix> {
    let text = std::fs::read_to_string(path)?;
    let report: KillMatrixReport = serde_json::from_str(&text)?;

    let mut mutant_index: HashMap<String, usize> = HashMap::new();
    for kills in report.tests.values() {
        for mutant_id in kills {
            let next = mutant_index.len();
            mutant_index.entry(mutant_id.clone()).or_insert(next);
        }
    }
    let universe = mutant_index.len().max(1);

    let mut test_ids: Vec<&String> = report.tests.keys().collect();
    test_ids.sort();

    let tests = test_ids
        .into_iter()
        .map(|id| {
            let mut bits = FixedBitSet::with_capacity(universe);
            for mutant_id in &report.tests[id] {
                bits.set(mutant_index[mutant_id], true);
            }
            TestCoverage {
                test_id: id.clone(),
                bits,
            }
        })
        .collect();

    Ok(LoadedKillMatrix { tests })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tests_killing_the_same_mutants_get_identical_bitsets() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(
            br#"{"tests": {"test_a": ["m1", "m2"], "test_b": ["m1", "m2"], "test_c": ["m3"]}}"#,
        )
        .unwrap();
        let loaded = load(f.path()).unwrap();
        let by_id: HashMap<_, _> = loaded
            .tests
            .iter()
            .map(|t| (t.test_id.clone(), t.bits.clone()))
            .collect();
        assert_eq!(by_id["test_a"], by_id["test_b"]);
        assert_ne!(by_id["test_a"], by_id["test_c"]);
    }
}
