//! A baseline of known static violations, so CI fails only on new ones.
//!
//! An entry is the file, rule and test name of a violation, not its line, so
//! it survives edits above it. The baseline is a plain sorted JSON list that
//! diffs well in review.

use crate::static_lint::rules::Violation;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// One known violation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Entry {
    /// The file the violation is in.
    pub file: String,
    /// The rule name, e.g. `"CLONE_TEST"`.
    pub rule: String,
    /// The test the violation is anchored to.
    pub test_name: String,
}

impl Entry {
    fn of(v: &Violation) -> Self {
        Self {
            file: v.file.clone(),
            rule: v.rule.to_string(),
            test_name: v.test_name.clone(),
        }
    }
}

/// Writes the baseline for `violations` to `path`, sorted and deduplicated.
///
/// # Errors
/// Returns an error if the file can't be written.
pub fn write(path: &Path, violations: &[Violation]) -> anyhow::Result<usize> {
    let mut entries: Vec<Entry> = violations.iter().map(Entry::of).collect();
    entries.sort();
    entries.dedup();
    std::fs::write(path, serde_json::to_string_pretty(&entries)? + "\n")?;
    Ok(entries.len())
}

/// Reads a baseline written by [`write`].
///
/// # Errors
/// Returns an error if the file is missing or isn't a baseline.
pub fn load(path: &Path) -> anyhow::Result<HashSet<Entry>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read baseline {}: {e}", path.display()))?;
    let entries: Vec<Entry> = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} is not a dupdup baseline: {e}", path.display()))?;
    Ok(entries.into_iter().collect())
}

/// What a baseline leaves of a run.
pub struct Filtered {
    /// Violations not in the baseline.
    pub new: Vec<Violation>,
    /// How many violations the baseline absorbed.
    pub known: usize,
    /// Baseline entries that no longer occur: debt that has been paid down.
    pub fixed: usize,
}

/// Splits `violations` into those the baseline doesn't know and those it does.
#[must_use]
pub fn filter(violations: Vec<Violation>, baseline: &HashSet<Entry>) -> Filtered {
    let mut seen: HashSet<Entry> = HashSet::new();
    let mut new = Vec::new();
    let mut known = 0;
    for v in violations {
        let entry = Entry::of(&v);
        if baseline.contains(&entry) {
            known += 1;
            seen.insert(entry);
        } else {
            new.push(v);
        }
    }
    Filtered {
        new,
        known,
        fixed: baseline.len() - seen.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_lint::rules::Severity;

    fn violation(file: &str, rule: &'static str, test: &str, line: usize) -> Violation {
        Violation {
            file: file.into(),
            line,
            severity: Severity::Warn,
            rule,
            test_name: test.into(),
            message: String::new(),
        }
    }

    #[test]
    fn a_written_baseline_is_sorted_deduplicated_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let vs = vec![
            violation("b.py", "CLONE_TEST", "test_b", 3),
            violation("a.py", "NO_ASSERTIONS_SMOKE", "test_a", 9),
            violation("a.py", "NO_ASSERTIONS_SMOKE", "test_a", 20),
        ];
        assert_eq!(write(&path, &vs).unwrap(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.find("a.py").unwrap() < text.find("b.py").unwrap());
        assert_eq!(load(&path).unwrap().len(), 2);
    }

    #[test]
    fn known_violations_are_absorbed_even_when_their_line_moves() {
        let baseline: HashSet<Entry> = [Entry::of(&violation("a.py", "CLONE_TEST", "test_a", 5))]
            .into_iter()
            .collect();
        let out = filter(
            vec![
                violation("a.py", "CLONE_TEST", "test_a", 50),
                violation("a.py", "CLONE_TEST", "test_new", 60),
            ],
            &baseline,
        );
        assert_eq!(out.known, 1);
        assert_eq!(out.new.len(), 1);
        assert_eq!(out.new[0].test_name, "test_new");
        assert_eq!(out.fixed, 0);
    }

    #[test]
    fn baseline_entries_that_no_longer_occur_are_counted_as_fixed() {
        let baseline: HashSet<Entry> = [
            Entry::of(&violation("a.py", "CLONE_TEST", "test_a", 1)),
            Entry::of(&violation("a.py", "CLONE_TEST", "test_gone", 1)),
        ]
        .into_iter()
        .collect();
        let out = filter(
            vec![violation("a.py", "CLONE_TEST", "test_a", 1)],
            &baseline,
        );
        assert_eq!(out.fixed, 1);
    }

    #[test]
    fn a_missing_baseline_is_an_error_not_an_empty_one() {
        assert!(load(Path::new("/nonexistent/baseline.json")).is_err());
    }
}
