//! Adds what each test asserts to its requirement set, so two tests only
//! count as redundant if they cover the same code *and* check the same things.

use crate::redundancy::TestCoverage;
use crate::static_lint::{assertion_fingerprints, AssertingHelpers};
use std::collections::HashMap;
use std::path::Path;

/// Each test's assertion fingerprint, and how many tests couldn't be
/// resolved to source.
#[derive(Debug, Default)]
pub struct Fingerprints {
    /// Fingerprint per test id; a resolved test with no assertion maps to an
    /// empty list.
    pub by_test: HashMap<String, Vec<String>>,
    /// Tests whose source file or function wasn't found under the root.
    pub unresolved: usize,
}

/// Splits a pytest node id into its file and the class-qualified function
/// path, dropping the parametrize suffix: `a/t.py::C::test_x[p]` gives
/// (`a/t.py`, `C::test_x`). `None` for an id with no `::`.
fn split_node_id(test_id: &str) -> Option<(&str, &str)> {
    let head = match test_id.find('[') {
        Some(i) => &test_id[..i],
        None => test_id,
    };
    head.split_once("::")
}

/// Resolves every id in `test_ids` against the source under `root` and
/// fingerprints it. Each distinct file is parsed once.
#[must_use]
pub fn load<'a>(
    test_ids: impl IntoIterator<Item = &'a str>,
    root: &Path,
    helpers: &AssertingHelpers,
) -> Fingerprints {
    let mut per_file: HashMap<String, Option<HashMap<String, Vec<String>>>> = HashMap::new();
    let mut out = Fingerprints::default();
    for id in test_ids {
        let found = split_node_id(id).and_then(|(file, key)| {
            let fps = per_file.entry(file.to_string()).or_insert_with(|| {
                let source = std::fs::read_to_string(root.join(file)).ok()?;
                assertion_fingerprints(&source, helpers).ok()
            });
            fps.as_ref()?.get(key).cloned()
        });
        match found {
            Some(fp) => {
                out.by_test.insert(id.to_string(), fp);
            }
            None => out.unresolved += 1,
        }
    }
    out
}

/// Appends one requirement per distinct (file, assertion) to each test's
/// bitset: a test gets the bits of the assertions it makes. A test that
/// asserts nothing gets a requirement of its own: "does not raise" is its
/// whole check, so no other test can stand in for it. A test that couldn't
/// be resolved gains no bits.
pub fn augment(tests: &mut [TestCoverage], fingerprints: &Fingerprints) {
    let mut index: HashMap<(String, &str), usize> = HashMap::new();
    let base = tests.first().map_or(0, |t| t.bits.len());
    let mut per_test: Vec<Vec<usize>> = Vec::with_capacity(tests.len());
    for t in tests.iter() {
        let file = t.test_id.split("::").next().unwrap_or("").to_string();
        let mut bits: Vec<usize> = Vec::new();
        if let Some(fp) = fingerprints.by_test.get(&t.test_id) {
            if fp.is_empty() {
                // A key no real fingerprint can equal: it never contains a
                // test id's own text for an assertion.
                let next = base + index.len();
                bits.push(*index.entry((t.test_id.clone(), "")).or_insert(next));
            }
            for text in fp {
                let next = base + index.len();
                bits.push(*index.entry((file.clone(), text.as_str())).or_insert(next));
            }
        }
        per_test.push(bits);
    }
    let total = base + index.len();
    for (t, bits) in tests.iter_mut().zip(per_test) {
        t.bits.grow(total);
        for b in bits {
            t.bits.insert(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixedbitset::FixedBitSet;

    fn tc(id: &str, lines: &[usize]) -> TestCoverage {
        let mut bits = FixedBitSet::with_capacity(4);
        for &l in lines {
            bits.insert(l);
        }
        TestCoverage {
            test_id: id.into(),
            bits,
        }
    }

    fn fps(pairs: &[(&str, &[&str])]) -> Fingerprints {
        Fingerprints {
            by_test: pairs
                .iter()
                .map(|(id, a)| {
                    (
                        (*id).to_string(),
                        a.iter().map(|s| (*s).to_string()).collect(),
                    )
                })
                .collect(),
            unresolved: 0,
        }
    }

    #[test]
    fn split_node_id_drops_the_parametrize_suffix() {
        assert_eq!(
            split_node_id("a/t.py::C::test_x[p::q]"),
            Some(("a/t.py", "C::test_x"))
        );
        assert_eq!(split_node_id("bare"), None);
    }

    #[test]
    fn tests_with_different_assertions_stop_being_duplicates() {
        let mut tests = vec![tc("f.py::test_a", &[1, 2]), tc("f.py::test_b", &[1, 2])];
        augment(
            &mut tests,
            &fps(&[
                ("f.py::test_a", &["assert x == 1"]),
                ("f.py::test_b", &["assert x == 2"]),
            ]),
        );
        assert!(crate::redundancy::analyze(&tests).is_empty());
    }

    #[test]
    fn tests_with_the_same_assertions_stay_duplicates() {
        let mut tests = vec![tc("f.py::test_a", &[1, 2]), tc("f.py::test_b", &[1, 2])];
        augment(
            &mut tests,
            &fps(&[
                ("f.py::test_a", &["assert x == 1"]),
                ("f.py::test_b", &["assert x == 1"]),
            ]),
        );
        assert_eq!(crate::redundancy::analyze(&tests).len(), 1);
    }

    #[test]
    fn a_subset_of_the_assertions_is_still_subsumed() {
        let mut tests = vec![tc("f.py::test_a", &[1]), tc("f.py::test_b", &[1, 2])];
        augment(
            &mut tests,
            &fps(&[
                ("f.py::test_a", &["assert x"]),
                ("f.py::test_b", &["assert x", "assert y"]),
            ]),
        );
        let findings = crate::redundancy::analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].test_id, "f.py::test_a");
    }

    #[test]
    fn a_test_that_asserts_nothing_is_not_subsumed_by_one_that_does() {
        let mut tests = vec![tc("f.py::test_quiet", &[1]), tc("f.py::test_loud", &[1, 2])];
        augment(
            &mut tests,
            &fps(&[
                ("f.py::test_quiet", &[]),
                ("f.py::test_loud", &["assert x"]),
            ]),
        );
        assert!(crate::redundancy::analyze(&tests).is_empty());
    }

    #[test]
    fn load_resolves_class_methods_and_counts_the_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("t.py"),
            "class TestC:\n    def test_m(self):\n        assert 1\n",
        )
        .unwrap();
        let loaded = load(
            [
                "t.py::TestC::test_m[a]",
                "t.py::test_gone",
                "missing.py::test_x",
            ],
            dir.path(),
            &AssertingHelpers::default(),
        );
        assert_eq!(loaded.by_test["t.py::TestC::test_m[a]"], vec!["assert 1"]);
        assert_eq!(loaded.unresolved, 2);
    }
}
