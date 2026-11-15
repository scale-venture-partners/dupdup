//! Running mutants by forking, through the `dupdup_fork` pytest plugin.
//!
//! One pytest process imports the project and collects once, then forks a
//! worker per mutant that swaps the mutated function's code into the live
//! module, runs the candidate tests and reports which failed. The cost per
//! mutant is a fork and the tests, not an interpreter start and an import of
//! the whole project, which is what dominates a process-per-mutant run on a
//! project whose tests import its whole app.

use super::{credit, KillMatrix, PreparedSource, RunConfig};
use crate::dynamic::coverage::LineCoverage;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const PLUGIN: &str = include_str!("dupdup_fork.py");

/// Mutants per pytest process. Each process pays the project's import once, so
/// a larger chunk amortises it; a smaller one reports progress sooner.
const CHUNK: usize = 400;

#[derive(Serialize)]
struct Job<'a> {
    id: &'a str,
    file: String,
    start: usize,
    end: usize,
    replacement: &'a str,
    tests: Vec<String>,
}

#[derive(Deserialize)]
struct JobResult {
    id: String,
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
    killers: Vec<String>,
    evaluated: Vec<String>,
}

/// Tallies of a forked run, for the summary line.
#[derive(Default)]
pub(super) struct Tally {
    pub killed: usize,
    pub survived: usize,
    pub no_coverage: usize,
    pub inconclusive: usize,
    pub budget_hit: bool,
}

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs every mutant of `prepared` through the fork plugin, `cfg.workers` at a
/// time, in chunks, handing the matrix to `progress` after each chunk.
pub(super) fn collect_prepared(
    prepared: &[PreparedSource],
    coverage: &LineCoverage,
    cfg: &RunConfig,
    kills: &mut KillMatrix,
    progress: &mut dyn FnMut(&KillMatrix) -> anyhow::Result<()>,
) -> anyhow::Result<Tally> {
    let mut tally = Tally::default();
    let mut pending: Vec<(&PreparedSource, Vec<usize>)> = Vec::new();
    let mut pending_jobs = 0usize;
    let mut sources = prepared.iter().peekable();
    while let Some(source) = sources.next() {
        if source.mutants.is_empty() {
            kills.mutated.insert(source.key.clone(), 0);
            continue;
        }
        let mut with_tests = Vec::new();
        for (i, mutant) in source.mutants.iter().enumerate() {
            let mut candidates =
                coverage.tests_covering(&source.key, mutant.candidate_lines.clone());
            if let Some(narrow) = &cfg.narrow_candidates {
                candidates = narrow(candidates);
            }
            if candidates.is_empty() {
                tally.no_coverage += 1;
            } else {
                with_tests.push(i);
            }
        }
        pending_jobs += with_tests.len();
        pending.push((source, with_tests));
        if pending_jobs >= CHUNK || sources.peek().is_none() {
            if cfg.deadline.is_some_and(|d| Instant::now() > d) {
                tally.budget_hit = true;
                break;
            }
            run_chunk(&pending, coverage, cfg, kills, &mut tally)?;
            for (source, _) in &pending {
                kills
                    .mutated
                    .insert(source.key.clone(), source.mutants.len());
            }
            progress(kills)?;
            pending.clear();
            pending_jobs = 0;
        }
    }
    Ok(tally)
}

fn run_chunk(
    pending: &[(&PreparedSource, Vec<usize>)],
    coverage: &LineCoverage,
    cfg: &RunConfig,
    kills: &mut KillMatrix,
    tally: &mut Tally,
) -> anyhow::Result<()> {
    let mut jobs = Vec::new();
    let mut tests_to_collect: BTreeSet<String> = BTreeSet::new();
    for (source, indexes) in pending {
        for &i in indexes {
            let mutant = &source.mutants[i];
            let mut tests = coverage.tests_covering(&source.key, mutant.candidate_lines.clone());
            if let Some(narrow) = &cfg.narrow_candidates {
                tests = narrow(tests);
            }
            // The plugin selects every case of a test named without its
            // parametrize id, so name each test once.
            let tests: Vec<String> = tests
                .iter()
                .map(|t| t.split('[').next().unwrap_or(t).to_string())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            tests_to_collect.extend(tests.iter().cloned());
            let (start, end, replacement) = mutant.edit();
            jobs.push(Job {
                id: &mutant.id,
                file: source.path.to_string_lossy().into_owned(),
                start,
                end,
                replacement,
                tests,
            });
        }
    }
    if jobs.is_empty() {
        return Ok(());
    }

    let scratch = Scratch(std::env::temp_dir().join(format!(
        "dupdup-fork-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )));
    std::fs::create_dir_all(&scratch.0)?;
    std::fs::write(scratch.0.join("dupdup_fork.py"), PLUGIN)?;
    let jobs_file = scratch.0.join("jobs.json");
    let out_file = scratch.0.join("out.json");
    std::fs::write(
        &jobs_file,
        serde_json::to_string(&serde_json::json!({ "mutants": jobs }))?,
    )?;

    let python_path = match std::env::var("PYTHONPATH") {
        Ok(existing) if !existing.is_empty() => {
            format!("{}:{existing}", scratch.0.display())
        }
        _ => scratch.0.display().to_string(),
    };
    let deadline_epoch = cfg.deadline.map(|d| {
        let left = d.saturating_duration_since(Instant::now());
        (SystemTime::now() + left)
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |t| t.as_secs_f64())
    });
    let mut command = Command::new(&cfg.pytest_cmd[0]);
    command
        .args(&cfg.pytest_cmd[1..])
        .args(["-p", "dupdup_fork", "--dupdup-jobs"])
        .arg(&jobs_file)
        .arg("--dupdup-out")
        .arg(&out_file)
        .args(["--dupdup-workers", &cfg.workers.to_string()])
        .args(["--dupdup-timeout", &cfg.timeout.as_secs().to_string()])
        .args(&tests_to_collect)
        .env("PYTHONPATH", python_path)
        .envs(cfg.env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(epoch) = deadline_epoch {
        command.args(["--dupdup-deadline", &epoch.to_string()]);
    }
    if let Some(dir) = &cfg.cwd {
        command.current_dir(dir);
    }
    let started = Instant::now();
    let output = command
        .output()
        .context("failed to start pytest with the dupdup_fork plugin")?;
    let results: Vec<JobResult> = match std::fs::read_to_string(&out_file) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(_) => anyhow::bail!(
            "the dupdup_fork plugin produced no result (it needs Python 3.11+ and a platform with fork):\n{}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    eprintln!(
        "  {} mutant(s) run in {:.1}s",
        results.len(),
        started.elapsed().as_secs_f32()
    );

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for r in &results {
        seen.insert(&r.id);
        match r.outcome.as_str() {
            "killed" => {
                tally.killed += 1;
                credit(kills, &r.evaluated, &r.killers, &r.id);
            }
            "survived" => {
                tally.survived += 1;
                credit(kills, &r.evaluated, &[], &r.id);
            }
            other => {
                tally.inconclusive += 1;
                eprintln!(
                    "  {} inconclusive ({other}{}) — excluded",
                    r.id,
                    r.reason
                        .as_deref()
                        .map(|s| format!(": {s}"))
                        .unwrap_or_default()
                );
            }
        }
    }
    // Jobs the plugin never started (past its deadline) have no result.
    let missing = jobs.iter().filter(|j| !seen.contains(j.id)).count();
    if missing > 0 {
        tally.budget_hit = true;
    }
    Ok(())
}
