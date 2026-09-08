//! The bitset engine shared by both dynamic-tier loaders: given a set of
//! `(test_id, requirement_bitset)` pairs, `analyze` finds duplicate,
//! subsumed, and vacuous tests.

use fixedbitset::FixedBitSet;
use serde::Serialize;
use std::collections::HashSet;

/// One test's coverage of some requirement universe — covered source lines
/// for the coverage loader, killed mutant ids for the (future) mutation
/// loader. Both loaders produce this same shape so a single engine below
/// can find redundancy relations in either. `test_id` is assumed to be a
/// pytest node id (a file path, `::`, and a test name) — both loaders'
/// domains are pytest-based, and `analyze` uses that structure to scope
/// comparisons.
pub struct TestCoverage {
    /// The test's id, as produced by its loader (e.g. a pytest node id).
    pub test_id: String,
    /// The requirements (lines, or mutants) this test covers.
    pub bits: FixedBitSet,
}

/// A relation `analyze` found between one test and the rest of the suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancyKind {
    /// Identical requirement coverage to another, unrelated test.
    DuplicateCoverage,
    /// A strict subset of another single test's coverage.
    SubsumedCoverage,
    /// Every requirement this test covers is also covered by the union of
    /// every other test — removing it would shrink nothing.
    ContributesNothingUnique,
    /// Covers zero requirements (zero lines for the coverage source, zero
    /// killed mutants for the mutation source) — vacuously a subset of
    /// every other test, so reported once instead of once per peer.
    CoversNothing,
}

/// One test flagged by `analyze`, and why.
#[derive(Debug, Serialize)]
pub struct RedundancyFinding {
    /// Which relation was found.
    pub kind: RedundancyKind,
    /// The test the finding is about.
    pub test_id: String,
    /// The other test(s) it's redundant against, if the relation names one
    /// (empty for `ContributesNothingUnique` and `CoversNothing`, which are
    /// about the whole suite or nothing at all).
    pub related: Vec<String>,
}

/// Pytest's parametrize suffix (`test_foo[1-2]`). Siblings in the same
/// family are expected to share most or all coverage — that's the point of
/// parametrizing — so they're excluded from the duplicate/subsumed relations
/// below and left to the static tier's clone-detection rule instead.
#[must_use]
pub fn base_test_name(test_id: &str) -> &str {
    match test_id.find('[') {
        Some(idx) => &test_id[..idx],
        None => test_id,
    }
}

/// The file a pytest node id belongs to (everything before the first
/// `::`). `DuplicateCoverage`/`SubsumedCoverage`/`ContributesNothingUnique`
/// only ever compare tests in the same file: calibrating against a real
/// ~2000-test suite found that essentially every cross-file pairing was
/// two tests from unrelated feature areas that happened to share coverage
/// of common infrastructure (middleware, logging, a shared client) — not
/// genuine redundancy. Acting on those findings would delete the only
/// direct test of whichever behavior was "subsumed." This scoping doesn't
/// help within a single file (e.g. a set of boundary-condition tests that
/// all return through the same lines of a loop) — that needs branch-level
/// coverage data, which isn't available from `coverage json`'s report
/// format even with branch coverage turned on (confirmed empirically: its
/// per-line `contexts` map doesn't vary by which branch a line's context
/// took).
#[must_use]
pub fn test_file(test_id: &str) -> &str {
    match test_id.find("::") {
        Some(idx) => &test_id[..idx],
        None => test_id,
    }
}

fn is_subset(a: &FixedBitSet, b: &FixedBitSet) -> bool {
    let mut diff = a.clone();
    diff.difference_with(b);
    diff.count_ones(..) == 0
}

fn comparable(a: &str, b: &str) -> bool {
    base_test_name(a) != base_test_name(b) && test_file(a) == test_file(b)
}

/// Finds redundancy relations among `tests`. All of `tests` must share one
/// requirement universe (all from the same loader) — comparing bitsets from
/// different universes produces meaningless results.
#[must_use]
pub fn analyze(tests: &[TestCoverage]) -> Vec<RedundancyFinding> {
    let n = tests.len();
    let mut findings = Vec::new();
    let mut explained: HashSet<usize> = HashSet::new();

    for (i, test) in tests.iter().enumerate() {
        if test.bits.count_ones(..) == 0 {
            explained.insert(i);
            findings.push(RedundancyFinding {
                kind: RedundancyKind::CoversNothing,
                test_id: test.test_id.clone(),
                related: Vec::new(),
            });
        }
    }

    for i in 0..n {
        if explained.contains(&i) {
            continue;
        }
        for j in 0..n {
            if i == j || !comparable(&tests[i].test_id, &tests[j].test_id) {
                continue;
            }
            if tests[i].bits == tests[j].bits {
                explained.insert(i);
                if i < j {
                    findings.push(RedundancyFinding {
                        kind: RedundancyKind::DuplicateCoverage,
                        test_id: tests[i].test_id.clone(),
                        related: vec![tests[j].test_id.clone()],
                    });
                }
            } else if is_subset(&tests[i].bits, &tests[j].bits) {
                explained.insert(i);
                findings.push(RedundancyFinding {
                    kind: RedundancyKind::SubsumedCoverage,
                    test_id: tests[i].test_id.clone(),
                    related: vec![tests[j].test_id.clone()],
                });
            }
        }
    }

    for (i, test) in tests.iter().enumerate() {
        if explained.contains(&i) || test.bits.count_ones(..) == 0 {
            continue;
        }
        let mut union = test.bits.clone();
        union.clear();
        for (j, other) in tests.iter().enumerate() {
            if j != i && comparable(&other.test_id, &test.test_id) {
                union.union_with(&other.bits);
            }
        }
        if is_subset(&test.bits, &union) {
            findings.push(RedundancyFinding {
                kind: RedundancyKind::ContributesNothingUnique,
                test_id: test.test_id.clone(),
                related: Vec::new(),
            });
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(set: &[usize], len: usize) -> FixedBitSet {
        let mut b = FixedBitSet::with_capacity(len);
        for &i in set {
            b.set(i, true);
        }
        b
    }

    #[test]
    fn identical_coverage_is_flagged_duplicate() {
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_a".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "f.py::test_b".into(),
                bits: bits(&[1, 2], 5),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RedundancyKind::DuplicateCoverage);
        assert_eq!(findings[0].test_id, "f.py::test_a");
        assert_eq!(findings[0].related, vec!["f.py::test_b"]);
    }

    #[test]
    fn identical_coverage_in_different_files_is_not_flagged() {
        let tests = vec![
            TestCoverage {
                test_id: "a.py::test_x".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "b.py::test_y".into(),
                bits: bits(&[1, 2], 5),
            },
        ];
        assert!(analyze(&tests).is_empty());
    }

    #[test]
    fn subset_coverage_in_different_files_is_not_flagged() {
        let tests = vec![
            TestCoverage {
                test_id: "a.py::test_small".into(),
                bits: bits(&[1], 5),
            },
            TestCoverage {
                test_id: "b.py::test_big".into(),
                bits: bits(&[1, 2, 3], 5),
            },
        ];
        assert!(analyze(&tests).is_empty());
    }

    #[test]
    fn parametrize_siblings_with_identical_coverage_are_not_flagged() {
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_a[1]".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "f.py::test_a[2]".into(),
                bits: bits(&[1, 2], 5),
            },
        ];
        assert!(analyze(&tests).is_empty());
    }

    #[test]
    fn strict_subset_is_flagged_subsumed() {
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_small".into(),
                bits: bits(&[1], 5),
            },
            TestCoverage {
                test_id: "f.py::test_big".into(),
                bits: bits(&[1, 2, 3], 5),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RedundancyKind::SubsumedCoverage);
        assert_eq!(findings[0].test_id, "f.py::test_small");
        assert_eq!(findings[0].related, vec!["f.py::test_big"]);
    }

    #[test]
    fn unique_coverage_is_not_flagged() {
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_a".into(),
                bits: bits(&[1], 5),
            },
            TestCoverage {
                test_id: "f.py::test_b".into(),
                bits: bits(&[2], 5),
            },
        ];
        assert!(analyze(&tests).is_empty());
    }

    #[test]
    fn coverage_fully_reproduced_by_others_combined_is_flagged_redundant() {
        // test_a and test_b each carry a bit the other two tests never
        // touch, so neither is itself reproducible by the rest — only
        // test_wide's coverage is fully contained in the union of the
        // other two, isolating this relation from pairwise SubsumedCoverage.
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_wide".into(),
                bits: bits(&[1, 2], 12),
            },
            TestCoverage {
                test_id: "f.py::test_a".into(),
                bits: bits(&[1, 3, 10], 12),
            },
            TestCoverage {
                test_id: "f.py::test_b".into(),
                bits: bits(&[2, 3, 11], 12),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RedundancyKind::ContributesNothingUnique);
        assert_eq!(findings[0].test_id, "f.py::test_wide");
    }

    #[test]
    fn contributes_nothing_unique_does_not_count_a_different_file_as_evidence() {
        let tests = vec![
            TestCoverage {
                test_id: "a.py::test_wide".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "b.py::test_a".into(),
                bits: bits(&[1], 5),
            },
            TestCoverage {
                test_id: "b.py::test_b".into(),
                bits: bits(&[2], 5),
            },
        ];
        assert!(analyze(&tests).is_empty());
    }

    #[test]
    fn god_test_and_its_subsumed_siblings_are_all_reported() {
        // Here test_a and test_b are each a strict subset of test_wide, so
        // both relations fire: test_wide is redundant against the union of
        // the others, and test_a/test_b are each redundant against wide
        // alone. Both are legitimate, independently actionable findings.
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_wide".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "f.py::test_a".into(),
                bits: bits(&[1], 5),
            },
            TestCoverage {
                test_id: "f.py::test_b".into(),
                bits: bits(&[2], 5),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 3);
        assert!(findings.iter().any(|f| f.test_id == "f.py::test_wide"
            && f.kind == RedundancyKind::ContributesNothingUnique));
        assert!(findings
            .iter()
            .any(|f| f.test_id == "f.py::test_a" && f.kind == RedundancyKind::SubsumedCoverage));
        assert!(findings
            .iter()
            .any(|f| f.test_id == "f.py::test_b" && f.kind == RedundancyKind::SubsumedCoverage));
    }

    #[test]
    fn covers_nothing_is_reported_once_not_once_per_peer() {
        let tests = vec![
            TestCoverage {
                test_id: "f.py::test_zero".into(),
                bits: bits(&[], 5),
            },
            TestCoverage {
                test_id: "f.py::test_a".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "f.py::test_b".into(),
                bits: bits(&[3, 4], 5),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RedundancyKind::CoversNothing);
        assert_eq!(findings[0].test_id, "f.py::test_zero");
    }

    #[test]
    fn covers_nothing_fires_regardless_of_file() {
        let tests = vec![
            TestCoverage {
                test_id: "a.py::test_zero".into(),
                bits: bits(&[], 5),
            },
            TestCoverage {
                test_id: "b.py::test_a".into(),
                bits: bits(&[1, 2], 5),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RedundancyKind::CoversNothing);
    }
}
