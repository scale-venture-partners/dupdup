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
    /// a few other tests in its file (at most [`MAX_UNION_PEERS`], named in
    /// `related`) — removing it would shrink nothing. No single peer covers
    /// it, or it would be `SubsumedCoverage`.
    ContributesNothingUnique,
    /// Safe to remove together with every other `Removable` test without
    /// shrinking the coverage of its file. Unlike the relations above, which
    /// are per test, this set is jointly coverage-preserving. Set by
    /// `analyze --removal-set`, not by [`analyze`].
    Removable,
    /// Covers zero requirements (zero lines for the coverage source, zero
    /// killed mutants for the mutation source) — vacuously a subset of
    /// every other test, so reported once instead of once per peer.
    CoversNothing,
    /// The test ran, but its own body executed no line of the measured
    /// code. Set by `analyze --junit-xml`, not by [`analyze`]; its id is the
    /// junit `classname::name`.
    NoCoverage,
}

/// One test flagged by `analyze`, and why.
#[derive(Debug, Serialize)]
pub struct RedundancyFinding {
    /// Which relation was found.
    pub kind: RedundancyKind,
    /// The test the finding is about.
    pub test_id: String,
    /// The other test(s) it's redundant against, if the relation names one
    /// (empty for `CoversNothing`, which is about nothing at all).
    pub related: Vec<String>,
    /// How many further tests matched the same relation but aren't named in
    /// `related`: only the tightest superset is named for
    /// `SubsumedCoverage`, and every other duplicate is named for
    /// `DuplicateCoverage` already.
    pub other_matches: usize,
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
/// only ever compare tests in the same file when both ids look like pytest
/// node ids (see `comparable`): calibrating against a real
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

/// Whether two test ids are eligible to be compared against each other at
/// all: never the same base test (parametrize siblings), and — only when
/// both ids are actual pytest node ids (contain `::`) — the same file. A
/// mutation-source kill matrix isn't guaranteed to key by pytest node id
/// (the format documented in `dynamic::killmatrix` is an opaque `<test
/// id>` string); same-file scoping is a pytest-specific policy, so an id
/// set that doesn't look like pytest node ids falls back to comparing
/// every pair rather than silently scoping every pair out.
fn comparable(a: &str, b: &str) -> bool {
    if base_test_name(a) == base_test_name(b) {
        return false;
    }
    match (a.find("::"), b.find("::")) {
        (Some(_), Some(_)) => test_file(a) == test_file(b),
        _ => true,
    }
}

/// Finds redundancy relations among `tests`. All of `tests` must share one
/// requirement universe (all from the same loader) — comparing bitsets from
/// different universes produces meaningless results.
#[must_use]
pub fn analyze(tests: &[TestCoverage]) -> Vec<RedundancyFinding> {
    analyze_with(tests, true)
}

/// [`analyze`], with the `ContributesNothingUnique` relation optional. It is
/// the weakest of the four: each requirement can be covered by a different
/// peer, so "every requirement is covered somewhere else" rarely means the
/// test's scenario is covered.
#[must_use]
pub fn analyze_with(tests: &[TestCoverage], union_relation: bool) -> Vec<RedundancyFinding> {
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
                other_matches: 0,
            });
        }
    }

    for i in 0..n {
        if explained.contains(&i) {
            continue;
        }
        // Every comparable test with identical coverage, and the tightest
        // (fewest-bit) strict superset. One finding of each kind per test,
        // not one per peer.
        let mut twins: Vec<usize> = Vec::new();
        let mut supersets = 0usize;
        let mut tightest: Option<usize> = None;
        for j in 0..n {
            if i == j || !comparable(&tests[i].test_id, &tests[j].test_id) {
                continue;
            }
            if tests[i].bits == tests[j].bits {
                twins.push(j);
            } else if is_subset(&tests[i].bits, &tests[j].bits) {
                supersets += 1;
                if tightest
                    .is_none_or(|k| tests[j].bits.count_ones(..) < tests[k].bits.count_ones(..))
                {
                    tightest = Some(j);
                }
            }
        }
        if !twins.is_empty() {
            explained.insert(i);
            // The group is reported once, from its lowest-indexed member.
            if twins.iter().all(|&j| i < j) {
                findings.push(RedundancyFinding {
                    kind: RedundancyKind::DuplicateCoverage,
                    test_id: tests[i].test_id.clone(),
                    related: twins.iter().map(|&j| tests[j].test_id.clone()).collect(),
                    other_matches: 0,
                });
            }
        }
        if let Some(k) = tightest {
            explained.insert(i);
            findings.push(RedundancyFinding {
                kind: RedundancyKind::SubsumedCoverage,
                test_id: tests[i].test_id.clone(),
                related: vec![tests[k].test_id.clone()],
                other_matches: supersets - 1,
            });
        }
    }

    for (i, test) in tests.iter().enumerate() {
        if !union_relation || explained.contains(&i) || test.bits.count_ones(..) == 0 {
            continue;
        }
        let peers: Vec<usize> = (0..n)
            .filter(|&j| j != i && comparable(&tests[j].test_id, &test.test_id))
            .collect();
        if let Some(cover) = small_cover(tests, i, &peers) {
            findings.push(RedundancyFinding {
                kind: RedundancyKind::ContributesNothingUnique,
                test_id: test.test_id.clone(),
                related: cover.iter().map(|&j| tests[j].test_id.clone()).collect(),
                other_matches: 0,
            });
        }
    }

    findings
}

/// The most peers a `ContributesNothingUnique` finding may name. A larger
/// cover is true of nearly any test in a big file, since each requirement
/// can be covered by a different peer, and isn't a claim a reader can check.
const MAX_UNION_PEERS: usize = 3;

/// A set of at most [`MAX_UNION_PEERS`] of `peers` whose union covers every
/// requirement of `tests[target]`, chosen greedily (most uncovered
/// requirements first). `None` if no such set is found.
fn small_cover(tests: &[TestCoverage], target: usize, peers: &[usize]) -> Option<Vec<usize>> {
    let mut remaining = tests[target].bits.clone();
    let mut chosen = Vec::new();
    while remaining.count_ones(..) > 0 {
        if chosen.len() == MAX_UNION_PEERS {
            return None;
        }
        let best = peers
            .iter()
            .copied()
            .filter(|j| !chosen.contains(j))
            .map(|j| {
                let mut gain = remaining.clone();
                gain.intersect_with(&tests[j].bits);
                (gain.count_ones(..), j)
            })
            .filter(|(gain, _)| *gain > 0)
            .max_by_key(|(gain, j)| (*gain, std::cmp::Reverse(*j)))?;
        remaining.difference_with(&tests[best.1].bits);
        chosen.push(best.1);
    }
    Some(chosen)
}

/// A jointly coverage-preserving set of tests that can all be removed at once:
/// removing every returned test leaves the union of requirements covered
/// within each test's file unchanged, and so the whole suite's too. Greedy,
/// not minimum: smallest tests are considered first, and a test is taken only
/// if each of its requirements is still covered by a test that stays.
///
/// A parametrized test (`[` in its id) is never removed, since sharing
/// coverage is the point of parametrizing; it still counts as covering. Ids
/// that aren't pytest node ids are all scoped together.
///
/// This preserves coverage, not behaviour: two tests can cover the same
/// lines and check different things.
#[must_use]
pub fn removable_set(tests: &[TestCoverage]) -> Vec<String> {
    let mut by_scope: std::collections::BTreeMap<&str, Vec<usize>> = Default::default();
    for (i, t) in tests.iter().enumerate() {
        let scope = if t.test_id.contains("::") {
            test_file(&t.test_id)
        } else {
            ""
        };
        by_scope.entry(scope).or_default().push(i);
    }

    let mut removable = Vec::new();
    for members in by_scope.values() {
        let universe = tests[members[0]].bits.len();
        let mut count = vec![0usize; universe];
        for &i in members {
            for b in tests[i].bits.ones() {
                count[b] += 1;
            }
        }
        let mut order: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| !tests[i].test_id.contains('['))
            .collect();
        order.sort_by_key(|&i| (tests[i].bits.count_ones(..), tests[i].test_id.clone()));
        for i in order {
            if tests[i].bits.ones().all(|b| count[b] >= 2) {
                for b in tests[i].bits.ones() {
                    count[b] -= 1;
                }
                removable.push(tests[i].test_id.clone());
            }
        }
    }
    removable.sort();
    removable
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
    fn bare_ids_without_a_pytest_node_id_shape_are_not_file_scoped() {
        // A kill-matrix producer isn't guaranteed to key by pytest node id
        // (see `dynamic::killmatrix`'s documented `<test id>` contract) — a
        // bare id like "test_a" has no "::" to scope by, so same-file
        // scoping must not silently zero out every relation for this shape.
        let tests = vec![
            TestCoverage {
                test_id: "test_a".into(),
                bits: bits(&[1, 2], 5),
            },
            TestCoverage {
                test_id: "test_b".into(),
                bits: bits(&[1, 2], 5),
            },
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RedundancyKind::DuplicateCoverage);
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

    fn tc(id: &str, set: &[usize]) -> TestCoverage {
        TestCoverage {
            test_id: id.into(),
            bits: bits(set, 8),
        }
    }

    #[test]
    fn a_group_of_identical_tests_is_one_finding() {
        let tests = vec![
            tc("f.py::test_a", &[1, 2]),
            tc("f.py::test_b", &[1, 2]),
            tc("f.py::test_c", &[1, 2]),
        ];
        let findings = analyze(&tests);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].test_id, "f.py::test_a");
        assert_eq!(findings[0].related, vec!["f.py::test_b", "f.py::test_c"]);
    }

    #[test]
    fn a_subsumed_test_names_only_its_tightest_superset() {
        let tests = vec![
            tc("f.py::test_small", &[1]),
            tc("f.py::test_huge", &[1, 2, 3, 4]),
            tc("f.py::test_medium", &[1, 2]),
        ];
        let findings = analyze(&tests);
        let small = findings
            .iter()
            .find(|f| f.test_id == "f.py::test_small" && f.kind == RedundancyKind::SubsumedCoverage)
            .unwrap();
        assert_eq!(small.related, vec!["f.py::test_medium"]);
        assert_eq!(small.other_matches, 1);
    }

    #[test]
    fn removable_set_keeps_one_of_two_identical_tests() {
        let tests = vec![tc("f.py::test_a", &[1, 2]), tc("f.py::test_b", &[1, 2])];
        assert_eq!(removable_set(&tests), vec!["f.py::test_a"]);
    }

    #[test]
    fn removable_set_never_removes_a_unique_test() {
        let tests = vec![tc("f.py::test_a", &[1]), tc("f.py::test_b", &[2])];
        assert!(removable_set(&tests).is_empty());
    }

    #[test]
    fn removable_set_is_jointly_safe_for_overlapping_tests() {
        // Each of a, b, c is individually covered by the other two, but
        // removing a and b together would lose bit 1.
        let tests = vec![
            tc("f.py::test_a", &[1]),
            tc("f.py::test_b", &[1]),
            tc("f.py::test_c", &[1, 2]),
            tc("f.py::test_d", &[2]),
        ];
        let removed = removable_set(&tests);
        let kept: Vec<&TestCoverage> = tests
            .iter()
            .filter(|t| !removed.contains(&t.test_id))
            .collect();
        let mut union = bits(&[], 8);
        for t in kept {
            union.union_with(&t.bits);
        }
        assert!(union.contains(1) && union.contains(2));
        assert!(!removed.is_empty());
    }

    #[test]
    fn removable_set_never_removes_a_parametrized_test() {
        let tests = vec![tc("f.py::test_a[1]", &[1]), tc("f.py::test_a[2]", &[1])];
        assert!(removable_set(&tests).is_empty());
    }

    #[test]
    fn removable_set_scopes_coverage_to_the_file() {
        let tests = vec![tc("a.py::test_x", &[1]), tc("b.py::test_y", &[1])];
        assert!(removable_set(&tests).is_empty());
    }

    #[test]
    fn a_union_finding_names_the_peers_that_cover_the_test() {
        let tests = vec![
            tc("f.py::test_wide", &[1, 2]),
            tc("f.py::test_a", &[1, 3, 6]),
            tc("f.py::test_b", &[2, 4, 7]),
        ];
        let findings = analyze(&tests);
        let f = findings
            .iter()
            .find(|f| f.kind == RedundancyKind::ContributesNothingUnique)
            .unwrap();
        assert_eq!(f.test_id, "f.py::test_wide");
        assert_eq!(f.related, vec!["f.py::test_a", "f.py::test_b"]);
    }

    #[test]
    fn a_cover_needing_more_than_three_peers_is_not_reported() {
        let tests = vec![
            tc("f.py::test_wide", &[0, 1, 2, 3]),
            tc("f.py::test_a", &[0, 4]),
            tc("f.py::test_b", &[1, 5]),
            tc("f.py::test_c", &[2, 6]),
            tc("f.py::test_d", &[3, 7]),
        ];
        assert!(analyze(&tests)
            .iter()
            .all(|f| f.kind != RedundancyKind::ContributesNothingUnique));
    }

    #[test]
    fn the_union_relation_can_be_turned_off() {
        let tests = vec![
            tc("f.py::test_wide", &[1, 2]),
            tc("f.py::test_a", &[1, 3, 6]),
            tc("f.py::test_b", &[2, 4, 7]),
        ];
        assert!(analyze_with(&tests, true)
            .iter()
            .any(|f| f.kind == RedundancyKind::ContributesNothingUnique));
        assert!(analyze_with(&tests, false).is_empty());
    }
}
