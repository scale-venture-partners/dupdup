use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(repo_root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn repo_root(start: &Path) -> anyhow::Result<PathBuf> {
    let out = run(start, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

/// Files staged for commit (added/copied/modified), relative to repo root.
pub fn staged_files(repo_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let out = run(
        repo_root,
        &["diff", "--cached", "--name-only", "--diff-filter=ACM"],
    )?;
    Ok(out.lines().map(PathBuf::from).collect())
}

/// All git-tracked files, relative to repo root.
pub fn all_files(repo_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let out = run(repo_root, &["ls-files"])?;
    Ok(out.lines().map(PathBuf::from).collect())
}

/// The staged (index) content of `file`, i.e. what would actually be
/// committed — not the working-tree copy, which may have unstaged edits.
pub fn staged_content(repo_root: &Path, file: &Path) -> anyhow::Result<String> {
    let spec = format!(":{}", file.to_string_lossy());
    run(repo_root, &["show", &spec])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]).unwrap();
        run(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        run(dir.path(), &["config", "user.name", "test"]).unwrap();
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
        run(dir.path(), &["add", "staged.py"]).unwrap();

        let staged = staged_files(dir.path()).unwrap();
        assert_eq!(staged, vec![PathBuf::from("staged.py")]);
    }

    #[test]
    fn all_files_lists_every_tracked_file_staged_or_committed() {
        let dir = init_repo();
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        fs::write(dir.path().join("b.py"), "y = 2\n").unwrap();
        run(dir.path(), &["add", "a.py"]).unwrap();
        run(dir.path(), &["commit", "-q", "-m", "init"]).unwrap();
        run(dir.path(), &["add", "b.py"]).unwrap();

        let mut tracked = all_files(dir.path()).unwrap();
        tracked.sort();
        assert_eq!(tracked, vec![PathBuf::from("a.py"), PathBuf::from("b.py")]);
    }

    #[test]
    fn staged_content_reads_the_index_blob_not_the_working_tree() {
        let dir = init_repo();
        let path = dir.path().join("f.py");
        fs::write(&path, "before\n").unwrap();
        run(dir.path(), &["add", "f.py"]).unwrap();
        fs::write(&path, "after\n").unwrap();

        let content = staged_content(dir.path(), Path::new("f.py")).unwrap();
        assert_eq!(content, "before\n");
    }
}
