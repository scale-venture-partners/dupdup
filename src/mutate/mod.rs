//! A self-contained mutation-testing kill-matrix collector: generates
//! mutants via the same tree-sitter parser `static_lint` uses, and uses
//! the coverage source's per-line data to run only the tests that could
//! plausibly kill each mutant instead of the whole suite — the same
//! subprocess (`pytest`) the coverage workflow already assumes, no new
//! external tool.
//!
//! Mutating real source files in place is inherently risky if the process
//! is killed mid-run, so `collect` refuses to start unless every source
//! is clean in git first, and keeps an on-disk backup per file for the
//! duration — belt, suspenders, and a labeled exit for when both fail.

/// Mutant generation: the tree-sitter walk and the fixed operator table.
pub mod operators;

use crate::dynamic::coverage::LineCoverage;
use crate::git;
use anyhow::Context;
use operators::{find_mutants, Mutant};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How to invoke pytest, and how long to give each invocation before
/// giving up on attributing a mutant.
pub struct RunConfig {
    /// The base command, e.g. `["python", "-m", "pytest"]` — one or more
    /// pytest node ids are appended as trailing arguments per invocation.
    /// Never mutated with extra flags: the caller's own pytest config
    /// (addopts, markers) is left in full control.
    pub pytest_cmd: Vec<String>,
    /// Per-invocation wall-clock budget. A mutant that weakens a loop
    /// bound can hang the process it's applied to, and there's no way to
    /// tell "slow" from "infinite" from the outside — this is a hard cutoff.
    pub timeout: Duration,
}

enum Outcome {
    KilledBy(Vec<String>),
    Survived,
    NoCoverage,
    Inconclusive(&'static str),
}

fn command_string(cmd: &[String], node_ids: &[String]) -> String {
    cmd.iter()
        .chain(node_ids)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `pytest --collect-only` over every id in `node_ids` against the
/// unmodified source, so a node id that doesn't resolve — most likely
/// because `coverage`'s report is stale relative to the current test
/// suite — is caught before any file is mutated, rather than read out of
/// an ambiguous exit code while a mutant is live on disk (pytest uses the
/// same nonzero status for "this test failed" and "this test failed to
/// collect" once other tests in the same invocation did collect).
fn verify_node_ids(pytest_cmd: &[String], node_ids: &[String]) -> anyhow::Result<()> {
    if node_ids.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<&String> = node_ids.iter().collect();
    ids.sort_unstable();
    ids.dedup();
    let ids: Vec<String> = ids.into_iter().cloned().collect();

    let output = Command::new(&pytest_cmd[0])
        .args(&pytest_cmd[1..])
        .arg("--collect-only")
        .arg("-q")
        .args(&ids)
        .output()
        .with_context(|| format!("failed to run `{}`", command_string(pytest_cmd, &ids)))?;
    anyhow::ensure!(
        output.status.success(),
        "one or more test ids from the coverage report don't resolve under `{}` — the coverage \
         report may be stale relative to the current test suite:\n{}",
        command_string(pytest_cmd, &ids),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Runs `cmd` with `node_ids` appended, waiting up to `timeout`. `Ok(None)`
/// means it was killed for exceeding the timeout; otherwise the process's
/// own exit code.
fn run_pytest(
    cmd: &[String],
    node_ids: &[String],
    timeout: Duration,
) -> anyhow::Result<Option<i32>> {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(node_ids)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to run `{}`", command_string(cmd, node_ids)))?;

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs one mutant's candidate tests, having already confirmed via
/// `verify_node_ids` that every id in `candidates` is real: any nonzero
/// exit here is a genuine failure (or hang) under the mutant, never a bad
/// node id.
fn run_mutant(cfg: &RunConfig, candidates: &[String]) -> anyhow::Result<Outcome> {
    if candidates.is_empty() {
        return Ok(Outcome::NoCoverage);
    }
    match run_pytest(&cfg.pytest_cmd, candidates, cfg.timeout)? {
        Some(0) => Ok(Outcome::Survived),
        // Not drilled down: that would cost up to one more full timeout
        // per candidate, for every slow mutant.
        None => Ok(Outcome::Inconclusive("the batch run timed out")),
        Some(_) => {
            let mut killers = Vec::new();
            for id in candidates {
                let single = std::slice::from_ref(id);
                match run_pytest(&cfg.pytest_cmd, single, cfg.timeout)? {
                    Some(0) => {}
                    // Failed or hung alone: unambiguously this test's doing.
                    None | Some(_) => killers.push(id.clone()),
                }
            }
            if killers.is_empty() {
                Ok(Outcome::Inconclusive(
                    "the batch failed but no individual candidate reproduced it",
                ))
            } else {
                Ok(Outcome::KilledBy(killers))
            }
        }
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".dupdup-orig");
    PathBuf::from(name)
}

struct RestoreOnDrop {
    path: PathBuf,
    original: String,
}

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
    }
}

struct PreparedSource {
    key: String,
    path: PathBuf,
    original: String,
    mutants: Vec<Mutant>,
}

/// Records one conclusively evaluated mutant in `kills`. Every candidate test
/// gets an entry, so a test that was run against a mutant and never killed
/// one stays in the matrix with an empty list (and `analyze` can report it
/// as `COVERS_NOTHING`). Only `killers` are credited with `mutant_id`. A
/// test that never ran against an evaluated mutant gets no entry: "this test
/// could not have killed anything" isn't established for it.
fn credit(
    kills: &mut HashMap<String, Vec<String>>,
    candidates: &[String],
    killers: &[String],
    mutant_id: &str,
) {
    for t in candidates {
        kills.entry(t.clone()).or_default();
    }
    for t in killers {
        kills
            .entry(t.clone())
            .or_default()
            .push(mutant_id.to_string());
    }
}

/// Collects a kill matrix by mutating `sources` one at a time, restricting
/// each mutant's test run to what `coverage` says covers its enclosing
/// statement. Every source is validated up front — present in `coverage`,
/// clean in git, no leftover backup from an interrupted prior run — and
/// every candidate test id `coverage` names for any of them is confirmed
/// to actually resolve under the configured pytest command, all before
/// any file is touched — so a problem anywhere in the batch never leaves
/// an earlier source mutated while the run aborts.
///
/// # Errors
/// Returns an error if a source has no entry in `coverage`, has
/// uncommitted changes, has a leftover `.dupdup-orig` backup, can't be
/// read or parsed, or `coverage` names a test id that doesn't resolve —
/// a sign the coverage report is stale.
pub fn collect(
    repo_root: &Path,
    sources: &[PathBuf],
    coverage: &LineCoverage,
    cfg: &RunConfig,
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    anyhow::ensure!(!cfg.pytest_cmd.is_empty(), "pytest_cmd must not be empty");

    let mut prepared = Vec::with_capacity(sources.len());
    let mut all_candidates: HashSet<String> = HashSet::new();

    for source in sources {
        let key = source.to_string_lossy().into_owned();
        if !coverage.has_file(&key) {
            let known = coverage.known_files();
            let listed: Vec<&str> = known.iter().take(50).copied().collect();
            anyhow::bail!(
                "{key} has no entry in the coverage report — check it's spelled exactly as \
                 the report's own file keys are. Known files: {}{}",
                listed.join(", "),
                if known.len() > 50 { ", ..." } else { "" },
            );
        }
        if !git::is_clean(repo_root, source)? {
            anyhow::bail!(
                "{} has uncommitted changes — commit or stash it before mutating it: a clean \
                 exit restores it, but a killed process can leave the mutant on disk",
                source.display()
            );
        }
        let path = repo_root.join(source);
        let backup = backup_path(&path);
        if backup.exists() {
            anyhow::bail!(
                "{} already exists — a previous run on {} may have been interrupted before it \
                 could restore the original. Compare it and restore or delete it by hand first",
                backup.display(),
                source.display()
            );
        }

        let original = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", source.display()))?;
        let mutants = find_mutants(&key, &original)?;
        for mutant in &mutants {
            all_candidates.extend(coverage.tests_covering(&key, mutant.candidate_lines.clone()));
        }
        prepared.push(PreparedSource {
            key,
            path,
            original,
            mutants,
        });
    }

    let all_candidates: Vec<String> = all_candidates.into_iter().collect();
    verify_node_ids(&cfg.pytest_cmd, &all_candidates)?;

    let mut kills: HashMap<String, Vec<String>> = HashMap::new();
    let (mut killed, mut survived, mut no_coverage, mut inconclusive) =
        (0usize, 0usize, 0usize, 0usize);

    for source in &prepared {
        if source.mutants.is_empty() {
            eprintln!("{}: no mutation sites found", source.path.display());
            continue;
        }
        eprintln!(
            "{}: {} mutant(s)",
            source.path.display(),
            source.mutants.len()
        );

        let backup = backup_path(&source.path);
        std::fs::write(&backup, &source.original)
            .with_context(|| format!("failed to write backup {}", backup.display()))?;
        let _guard = RestoreOnDrop {
            path: source.path.clone(),
            original: source.original.clone(),
        };

        for mutant in &source.mutants {
            let candidates = coverage.tests_covering(&source.key, mutant.candidate_lines.clone());
            std::fs::write(&source.path, mutant.apply(&source.original))
                .with_context(|| format!("failed to write mutant to {}", source.path.display()))?;
            let outcome = run_mutant(cfg, &candidates);
            std::fs::write(&source.path, &source.original).with_context(|| {
                format!(
                    "failed to restore {} after mutating it",
                    source.path.display()
                )
            })?;

            match outcome? {
                Outcome::Survived => {
                    survived += 1;
                    eprintln!("  {} survived", mutant.id);
                    credit(&mut kills, &candidates, &[], &mutant.id);
                }
                Outcome::NoCoverage => {
                    no_coverage += 1;
                    eprintln!("  {} has no covering test — excluded", mutant.id);
                }
                Outcome::Inconclusive(reason) => {
                    inconclusive += 1;
                    eprintln!("  {} inconclusive ({reason}) — excluded", mutant.id);
                }
                Outcome::KilledBy(tests) => {
                    killed += 1;
                    eprintln!("  {} killed by {} test(s)", mutant.id, tests.len());
                    credit(&mut kills, &candidates, &tests, &mutant.id);
                }
            }
        }

        std::fs::remove_file(&backup)
            .with_context(|| format!("failed to remove backup {}", backup.display()))?;
    }

    eprintln!(
        "{killed} killed, {survived} survived, {no_coverage} with no coverage, \
         {inconclusive} inconclusive"
    );
    Ok(kills)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_keeps_a_candidate_that_killed_nothing_with_an_empty_list() {
        let mut kills = HashMap::new();
        let candidates = vec!["t.py::test_a".to_string(), "t.py::test_b".to_string()];
        credit(&mut kills, &candidates, &["t.py::test_a".to_string()], "m1");
        assert_eq!(kills["t.py::test_a"], vec!["m1"]);
        assert!(kills["t.py::test_b"].is_empty());
    }

    #[test]
    fn credit_for_a_survivor_still_registers_every_candidate() {
        let mut kills = HashMap::new();
        credit(&mut kills, &["t.py::test_a".to_string()], &[], "m1");
        assert!(kills["t.py::test_a"].is_empty());
    }

    #[test]
    fn credit_accumulates_kills_across_mutants() {
        let mut kills = HashMap::new();
        let a = vec!["t.py::test_a".to_string()];
        credit(&mut kills, &a, &a, "m1");
        credit(&mut kills, &a, &a, "m2");
        assert_eq!(kills["t.py::test_a"], vec!["m1", "m2"]);
    }

    #[test]
    fn backup_path_appends_a_suffix_to_the_full_filename() {
        assert_eq!(
            backup_path(Path::new("a/b.py")),
            PathBuf::from("a/b.py.dupdup-orig")
        );
    }

    #[test]
    fn restore_on_drop_restores_original_content_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.py");
        std::fs::write(&path, "original\n").unwrap();

        {
            let _guard = RestoreOnDrop {
                path: path.clone(),
                original: "original\n".to_string(),
            };
            std::fs::write(&path, "mutated\n").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "mutated\n");
        }

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }

    #[test]
    fn restore_on_drop_restores_even_when_the_scope_exits_via_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.py");
        std::fs::write(&path, "original\n").unwrap();
        let path_for_guard = path.clone();

        let result = std::panic::catch_unwind(move || {
            let _guard = RestoreOnDrop {
                path: path_for_guard.clone(),
                original: "original\n".to_string(),
            };
            std::fs::write(&path_for_guard, "mutated\n").unwrap();
            panic!("simulated failure while a mutant is live on disk");
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }
}
