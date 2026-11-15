//! mutmut's mutation operators, used for mutant generation only.
//!
//! A bundled helper runs in the project's own interpreter, imports mutmut as a
//! library, and returns each mutant as a byte-range edit. No `mutants/` copy,
//! stats run or mutmut run is involved, and the mutants can be restricted to
//! the lines a test covered, so a diff-scoped check gets mutmut-grade mutants
//! for just the functions it cares about.

use super::operators::Mutant;
use anyhow::Context;
use serde::Deserialize;
use std::io::Write;
use std::process::{Command, Stdio};

const HELPER: &str = include_str!("dupdup_mutants.py");

#[derive(Deserialize)]
struct Response {
    files: Vec<FileMutants>,
}

#[derive(Deserialize)]
struct FileMutants {
    mutants: Vec<Edit>,
}

#[derive(Deserialize)]
struct Edit {
    id: String,
    start: usize,
    end: usize,
    replacement: String,
    fn_start: usize,
    fn_end: usize,
}

/// Whether `python` (a command prefix such as `.venv/bin/python`) can import
/// mutmut.
#[must_use]
pub fn available(python: &[String]) -> bool {
    Command::new(&python[0])
        .args(&python[1..])
        .args(["-c", "import mutmut, libcst"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// mutmut's mutants for the file at `path`, restricted to `covered_lines` when
/// given. Each mutant's candidate lines are those of its enclosing function.
///
/// # Errors
/// Returns an error if the helper can't be run or its output isn't understood.
pub fn mutants_for(
    python: &[String],
    path: &str,
    covered_lines: Option<&[usize]>,
) -> anyhow::Result<Vec<Mutant>> {
    let scratch = std::env::temp_dir().join(format!(
        "dupdup-mutants-{}-{}.py",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::write(&scratch, HELPER)?;
    let result = run_helper(python, &scratch, path, covered_lines);
    let _ = std::fs::remove_file(&scratch);
    result
}

fn run_helper(
    python: &[String],
    helper: &std::path::Path,
    path: &str,
    covered_lines: Option<&[usize]>,
) -> anyhow::Result<Vec<Mutant>> {
    let request = serde_json::json!({"files": [{"file": path, "covered_lines": covered_lines}]});
    let mut child = Command::new(&python[0])
        .args(&python[1..])
        .arg(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start the mutmut mutant helper")?;
    child
        .stdin
        .take()
        .context("no stdin for the helper")?
        .write_all(request.to_string().as_bytes())?;
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "the mutmut mutant helper failed (is mutmut installed in that Python?):\n{}",
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    let response: Response = serde_json::from_slice(&output.stdout)
        .context("the mutmut mutant helper returned something that is not JSON")?;
    Ok(response
        .files
        .into_iter()
        .flat_map(|f| f.mutants)
        .map(|e| Mutant::from_edit(e.id, e.fn_start..=e.fn_end, e.start, e.end, e.replacement))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uv_python() -> Vec<String> {
        ["uv", "run", "--no-project", "--with", "mutmut", "python"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    #[test]
    fn mutmut_mutants_become_exact_byte_edits_inside_functions() {
        let python = uv_python();
        if !available(&python) {
            eprintln!("skipped: needs uv and mutmut");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.py");
        let source = "LIMIT = 10\n\n\ndef is_adult(age):\n    return age >= 18\n\n\ndef label(name):\n    return name.strip() + \"!\"\n";
        std::fs::write(&file, source).unwrap();
        let mutants = mutants_for(&python, file.to_str().unwrap(), None).unwrap();
        assert!(!mutants.is_empty());
        // Every edit applies cleanly and changes the source; none is the
        // module-level constant.
        for m in &mutants {
            let mutated = m.apply(source);
            assert_ne!(mutated, source, "{}", m.id);
            assert!(
                !m.candidate_lines.contains(&1),
                "module-level mutant offered: {}",
                m.id
            );
        }
        assert!(mutants.iter().any(|m| m.apply(source).contains("age > 18")));
    }

    #[test]
    fn covered_lines_restrict_the_mutants() {
        let python = uv_python();
        if !available(&python) {
            eprintln!("skipped: needs uv and mutmut");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.py");
        let source = "def a(x):\n    return x >= 1\n\n\ndef b(x):\n    return x >= 2\n";
        std::fs::write(&file, source).unwrap();
        let only_b = mutants_for(&python, file.to_str().unwrap(), Some(&[6])).unwrap();
        assert!(!only_b.is_empty());
        assert!(only_b.iter().all(|m| !m.apply(source).contains("x > 1")
            && !m.apply(source).contains("x >= 2\n")
            || m.apply(source).contains("x >= 1")));
        assert!(only_b.iter().all(|m| m.candidate_lines.start() >= &5));
    }
}
