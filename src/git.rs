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
