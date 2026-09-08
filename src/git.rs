//! Git plumbing via [`gix`], a pure-Rust implementation — dupdup never
//! shells out to a `git` binary at runtime, so it has no dependency on one
//! being installed or on its version/config matching what these functions
//! assume.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn open_repo(repo_root: &Path) -> Result<gix::Repository> {
    gix::open(repo_root).with_context(|| format!("{} is not a git repository", repo_root.display()))
}

/// The git repository root containing `start`.
///
/// # Errors
/// Returns an error if `start` isn't inside a git repository, or the
/// repository it's inside of has no working tree (e.g. a bare repository).
pub fn repo_root(start: &Path) -> Result<PathBuf> {
    let repo = gix::discover(start)
        .with_context(|| format!("{} is not inside a git repository", start.display()))?;
    repo.workdir()
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no working tree", start.display()))
}

/// Files staged for commit (added/copied/modified), relative to repo root.
///
/// # Errors
/// Returns an error if `repo_root` isn't a git repository.
pub fn staged_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let repo = open_repo(repo_root)?;
    let index = repo
        .index_or_empty()
        .context("failed to read the git index")?;
    let tree_id = repo
        .head_tree_id_or_empty()
        .context("failed to resolve HEAD's tree")?;

    let mut paths = Vec::new();
    repo.tree_index_status(
        &tree_id,
        &index,
        None,
        gix::status::tree_index::TrackRenames::Disabled,
        |change, _tree_index, _worktree_index| {
            match change {
                gix::diff::index::ChangeRef::Addition { location, .. }
                | gix::diff::index::ChangeRef::Modification { location, .. } => {
                    paths.push(gix::path::from_bstr(location).into_owned());
                }
                gix::diff::index::ChangeRef::Deletion { .. }
                | gix::diff::index::ChangeRef::Rewrite { .. } => {}
            }
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        },
    )
    .context("failed to diff the index against HEAD")?;
    Ok(paths)
}

/// All git-tracked files, relative to repo root.
///
/// # Errors
/// Returns an error if `repo_root` isn't a git repository.
pub fn all_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let repo = open_repo(repo_root)?;
    let index = repo
        .index_or_empty()
        .context("failed to read the git index")?;
    Ok(index
        .entries()
        .iter()
        .map(|entry| gix::path::from_bstr(entry.path(&index)).into_owned())
        .collect())
}

/// The staged (index) content of `file`, i.e. what would actually be
/// committed — not the working-tree copy, which may have unstaged edits.
///
/// # Errors
/// Returns an error if `repo_root` isn't a git repository or `file` isn't
/// staged.
pub fn staged_content(repo_root: &Path, file: &Path) -> Result<String> {
    let repo = open_repo(repo_root)?;
    let index = repo
        .index_or_empty()
        .context("failed to read the git index")?;
    let rel_path = gix::path::into_bstr(file);
    let entry = index
        .entry_by_path(&rel_path)
        .with_context(|| format!("{} is not staged", file.display()))?;
    let blob = repo
        .find_blob(entry.id)
        .with_context(|| format!("failed to read the staged blob for {}", file.display()))?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    // Test fixtures are built with the real `git` binary — that's setting
    // up realistic on-disk state to exercise against, not a dependency of
    // the functions under test, which never invoke `git` themselves.
    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "`git {}` failed", args.join(" "));
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        run(dir.path(), &["config", "user.email", "test@example.com"]);
        run(dir.path(), &["config", "user.name", "test"]);
        dir
    }

    #[test]
    fn repo_root_resolves_to_the_git_toplevel() {
        let dir = init_repo();
        let resolved = repo_root(dir.path()).unwrap();
        assert_eq!(
            fs::canonicalize(&resolved).unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn staged_files_lists_only_what_was_added() {
        let dir = init_repo();
        fs::write(dir.path().join("staged.py"), "x = 1\n").unwrap();
        fs::write(dir.path().join("unstaged.py"), "y = 2\n").unwrap();
        run(dir.path(), &["add", "staged.py"]);

        let staged = staged_files(dir.path()).unwrap();
        assert_eq!(staged, vec![PathBuf::from("staged.py")]);
    }

    #[test]
    fn staged_files_is_empty_before_any_commit_or_add() {
        let dir = init_repo();
        fs::write(dir.path().join("untracked.py"), "x = 1\n").unwrap();
        assert!(staged_files(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn staged_files_detects_a_modification_to_a_committed_file() {
        let dir = init_repo();
        let path = dir.path().join("f.py");
        fs::write(&path, "before\n").unwrap();
        run(dir.path(), &["add", "f.py"]);
        run(dir.path(), &["commit", "-q", "-m", "init"]);
        fs::write(&path, "after\n").unwrap();
        run(dir.path(), &["add", "f.py"]);

        let staged = staged_files(dir.path()).unwrap();
        assert_eq!(staged, vec![PathBuf::from("f.py")]);
    }

    #[test]
    fn all_files_lists_every_tracked_file_staged_or_committed() {
        let dir = init_repo();
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        fs::write(dir.path().join("b.py"), "y = 2\n").unwrap();
        run(dir.path(), &["add", "a.py"]);
        run(dir.path(), &["commit", "-q", "-m", "init"]);
        run(dir.path(), &["add", "b.py"]);

        let mut tracked = all_files(dir.path()).unwrap();
        tracked.sort();
        assert_eq!(tracked, vec![PathBuf::from("a.py"), PathBuf::from("b.py")]);
    }

    #[test]
    fn staged_content_reads_the_index_blob_not_the_working_tree() {
        let dir = init_repo();
        let path = dir.path().join("f.py");
        fs::write(&path, "before\n").unwrap();
        run(dir.path(), &["add", "f.py"]);
        fs::write(&path, "after\n").unwrap();

        let content = staged_content(dir.path(), Path::new("f.py")).unwrap();
        assert_eq!(content, "before\n");
    }

    #[test]
    fn staged_content_errors_on_a_file_that_isnt_staged() {
        let dir = init_repo();
        fs::write(dir.path().join("f.py"), "x = 1\n").unwrap();
        assert!(staged_content(dir.path(), Path::new("f.py")).is_err());
    }
}
