//! The tests a change adds or edits, found by comparing each test file's
//! functions at a base revision with the working tree.

use crate::{git, static_lint};
use std::path::{Path, PathBuf};

/// Whether a touched test is new or an edit of an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not in the base revision under this name.
    New,
    /// In the base revision with a different definition.
    Changed,
}

/// One test a change adds or edits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TouchedTest {
    /// A pytest node id without parametrize ids, so pytest selects every case
    /// of it: `tests/test_a.py::TestB::test_c`.
    pub node_id: String,
    /// Whether it is new or changed.
    pub status: Status,
}

/// The tests in `test_files` (relative to `repo_root`) that differ from
/// `base`, sorted by node id. A test only in the base (deleted) is ignored,
/// and a test file that does not exist at `base` makes all its tests new.
///
/// # Errors
/// Returns an error if `base` doesn't resolve or a file can't be read or
/// parsed.
pub fn touched_tests(
    repo_root: &Path,
    base: &str,
    test_files: &[PathBuf],
) -> anyhow::Result<Vec<TouchedTest>> {
    let mut out = Vec::new();
    for file in test_files {
        let current = std::fs::read_to_string(repo_root.join(file))?;
        let before = git::file_at(repo_root, base, file)?;
        if before.as_deref() == Some(current.as_str()) {
            continue;
        }
        let now = static_lint::test_sources(&current)?;
        let then = match &before {
            Some(text) => static_lint::test_sources(text)?,
            None => Default::default(),
        };
        let rel = file.to_string_lossy();
        for (key, text) in now {
            let status = match then.get(&key) {
                None => Status::New,
                Some(old) if *old != text => Status::Changed,
                Some(_) => continue,
            };
            out.push(TouchedTest {
                node_id: format!("{rel}::{key}"),
                status,
            });
        }
    }
    out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    fn repo_with(initial: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        run(dir.path(), &["config", "user.email", "t@example.com"]);
        run(dir.path(), &["config", "user.name", "t"]);
        fs::write(dir.path().join("test_a.py"), initial).unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    const BASE: &str = "def test_same():\n    assert f(1) == 2\n\nclass TestC:\n    def test_edit(self):\n        assert g() == 1\n";

    #[test]
    fn a_new_test_and_an_edited_test_are_found_and_an_untouched_one_is_not() {
        let dir = repo_with(BASE);
        let edited = "def test_same():\n    assert f(1) == 2\n\nclass TestC:\n    def test_edit(self):\n        assert g() == 2\n\ndef test_added():\n    assert h()\n";
        fs::write(dir.path().join("test_a.py"), edited).unwrap();
        let touched = touched_tests(dir.path(), "HEAD", &[PathBuf::from("test_a.py")]).unwrap();
        let got: Vec<(&str, Status)> = touched
            .iter()
            .map(|t| (t.node_id.as_str(), t.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("test_a.py::TestC::test_edit", Status::Changed),
                ("test_a.py::test_added", Status::New),
            ]
        );
    }

    #[test]
    fn whitespace_and_comment_edits_do_not_touch_a_test() {
        let dir = repo_with(BASE);
        let reformatted = "def test_same():\n    # a comment\n    assert f( 1 ) == 2\n\nclass TestC:\n    def test_edit(self):\n        assert g() == 1\n";
        fs::write(dir.path().join("test_a.py"), reformatted).unwrap();
        let touched = touched_tests(dir.path(), "HEAD", &[PathBuf::from("test_a.py")]).unwrap();
        assert!(touched.is_empty(), "{touched:?}");
    }

    #[test]
    fn a_file_missing_at_the_base_is_all_new() {
        let dir = repo_with(BASE);
        fs::write(
            dir.path().join("test_b.py"),
            "def test_x():\n    assert 1\n",
        )
        .unwrap();
        let touched = touched_tests(dir.path(), "HEAD", &[PathBuf::from("test_b.py")]).unwrap();
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].status, Status::New);
    }

    #[test]
    fn an_unknown_base_is_an_error() {
        let dir = repo_with(BASE);
        fs::write(
            dir.path().join("test_a.py"),
            "def test_x():\n    assert 1\n",
        )
        .unwrap();
        assert!(touched_tests(dir.path(), "nope", &[PathBuf::from("test_a.py")]).is_err());
    }
}
