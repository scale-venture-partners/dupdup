//! Attribution for mutants that [mutmut](https://github.com/boxed/mutmut) 3
//! already generated.
//!
//! mutmut writes every mutant of a function into a copy of the source under
//! `mutants/`, selects one at run time with the `MUTANT_UNDER_TEST`
//! environment variable, knows which tests cover each function
//! (its stats file) and records only whether each mutant died (one meta file
//! per source), not which test killed it. This module reads those
//! files and runs each mutant against its covering tests with a JUnit report,
//! so a failure is attributed to the tests that failed. dupdup brings no
//! mutation operators of its own here; mutmut's are used as they are.

use super::{credit, run_mutant, KillMatrix, Outcome, RunConfig};
use anyhow::Context;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Deserialize)]
struct Stats {
    tests_by_mangled_function_name: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct Meta {
    exit_code_by_key: HashMap<String, serde_json::Value>,
}

/// What one worker produced for a mutant: its index, its candidate tests,
/// and how the run went.
type Attributed = (usize, Vec<String>, anyhow::Result<Outcome>);

/// One mutant mutmut generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutmutMutant {
    /// mutmut's name for it, e.g. `pkg.mod.x_func__mutmut_3`; the value of
    /// `MUTANT_UNDER_TEST` that selects it.
    pub key: String,
    /// The mutated function's mangled name, e.g. `pkg.mod.x_func`, which keys
    /// the covering-test list.
    pub function: String,
    /// The source file, relative to the project root.
    pub source: String,
}

/// Everything read from a mutmut run: its mutants by source file, and which
/// tests cover each function.
pub struct MutmutRun {
    /// Mutants grouped by source file, sorted by file and key.
    pub by_source: BTreeMap<String, Vec<MutmutMutant>>,
    /// Covering tests (pytest node ids) per mangled function name.
    pub tests_by_function: HashMap<String, Vec<String>>,
}

/// The function a mutant key belongs to: `pkg.mod.x_func__mutmut_3` becomes
/// `pkg.mod.x_func`.
#[must_use]
pub fn function_of(key: &str) -> &str {
    key.rsplit_once("__mutmut_").map_or(key, |(f, _)| f)
}

fn find_meta_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            find_meta_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "meta") {
            out.push(path);
        }
    }
    Ok(())
}

/// Reads a mutmut run from the `mutants` directory under `root`.
///
/// # Errors
/// Returns an error if the stats file or a meta file is missing or malformed.
pub fn load(root: &Path) -> anyhow::Result<MutmutRun> {
    let mutants_dir = root.join("mutants");
    let stats_path = mutants_dir.join("mutmut-stats.json");
    let stats: Stats =
        serde_json::from_str(&std::fs::read_to_string(&stats_path).with_context(|| {
            format!(
                "failed to read {}; run `mutmut run` first",
                stats_path.display()
            )
        })?)
        .with_context(|| format!("{} is not mutmut stats JSON", stats_path.display()))?;

    let mut metas = Vec::new();
    find_meta_files(&mutants_dir, &mut metas)?;
    anyhow::ensure!(
        !metas.is_empty(),
        "no .meta files under {}; run `mutmut run` first",
        mutants_dir.display()
    );
    let mut by_source: BTreeMap<String, Vec<MutmutMutant>> = BTreeMap::new();
    for meta_path in metas {
        let meta: Meta = serde_json::from_str(&std::fs::read_to_string(&meta_path)?)
            .with_context(|| format!("{} is not a mutmut meta file", meta_path.display()))?;
        let relative = meta_path
            .strip_prefix(&mutants_dir)?
            .to_string_lossy()
            .into_owned();
        let source = relative
            .strip_suffix(".meta")
            .unwrap_or(&relative)
            .to_string();
        let mut mutants: Vec<MutmutMutant> = meta
            .exit_code_by_key
            .keys()
            .map(|key| MutmutMutant {
                key: key.clone(),
                function: function_of(key).to_string(),
                source: source.clone(),
            })
            .collect();
        mutants.sort_by(|a, b| a.key.cmp(&b.key));
        by_source.entry(source).or_default().extend(mutants);
    }
    Ok(MutmutRun {
        by_source,
        tests_by_function: stats.tests_by_mangled_function_name,
    })
}

/// Runs every mutant of `run` against the tests mutmut says cover its
/// function, `jobs` at a time, and records which tests failed. The matrix is
/// handed to `progress` after each source file, as in [`super::collect`], and
/// sources already in `resume`'s `mutated` are skipped.
///
/// A mutant with no covering test is skipped, like a no-coverage mutant in
/// `collect`.
///
/// # Errors
/// Returns an error if pytest can't be started or `progress` fails.
pub fn collect(
    root: &Path,
    run: &MutmutRun,
    cfg: &RunConfig,
    jobs: usize,
    resume: Option<KillMatrix>,
    progress: &mut dyn FnMut(&KillMatrix) -> anyhow::Result<()>,
) -> anyhow::Result<KillMatrix> {
    anyhow::ensure!(!cfg.pytest_cmd.is_empty(), "pytest_cmd must not be empty");
    let mut kills = resume.unwrap_or_default();
    let mutants_dir = root.join("mutants");
    let (mut killed, mut survived, mut no_coverage, mut inconclusive) =
        (0usize, 0usize, 0usize, 0usize);

    for (source, mutants) in &run.by_source {
        if kills.mutated.contains_key(source) {
            eprintln!("{source}: already attributed, skipping (resume)");
            continue;
        }
        eprintln!("{source}: {} mutant(s)", mutants.len());
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Attributed>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..jobs.max(1) {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(mutant) = mutants.get(i) else { break };
                    let candidates = run
                        .tests_by_function
                        .get(&mutant.function)
                        .cloned()
                        .unwrap_or_default();
                    let mut worker = cfg.clone();
                    worker.cwd = Some(mutants_dir.clone());
                    worker
                        .env
                        .push(("MUTANT_UNDER_TEST".to_string(), mutant.key.clone()));
                    let outcome = run_mutant(&worker, &candidates);
                    results.lock().unwrap().push((i, candidates, outcome));
                });
            }
        });
        let mut results = results.into_inner().unwrap();
        results.sort_by_key(|(i, _, _)| *i);
        for (i, candidates, outcome) in results {
            let id = &mutants[i].key;
            match outcome? {
                Outcome::Survived => {
                    survived += 1;
                    eprintln!("  {id} survived");
                    credit(&mut kills, &candidates, &[], id);
                }
                Outcome::NoCoverage => {
                    no_coverage += 1;
                    eprintln!("  {id} has no covering test — excluded");
                }
                Outcome::Inconclusive(reason) => {
                    inconclusive += 1;
                    eprintln!("  {id} inconclusive ({reason}) — excluded");
                }
                Outcome::KilledBy(tests) => {
                    killed += 1;
                    eprintln!("  {id} killed by {} test(s)", tests.len());
                    credit(&mut kills, &candidates, &tests, id);
                }
            }
        }
        kills.mutated.insert(source.clone(), mutants.len());
        progress(&kills)?;
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

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_mutant_key_names_its_function() {
        assert_eq!(
            function_of("toy.rules.x_clamp__mutmut_2"),
            "toy.rules.x_clamp"
        );
        assert_eq!(function_of("pkg.m.xǁCǁmeth__mutmut_10"), "pkg.m.xǁCǁmeth");
        assert_eq!(function_of("no_suffix"), "no_suffix");
    }

    #[test]
    fn a_mutmut_run_is_read_back_by_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("mutants");
        write(
            &m.join("mutmut-stats.json"),
            r#"{"tests_by_mangled_function_name": {"toy.rules.x_clamp": ["tests/t.py::a", "tests/t.py::b"]}}"#,
        );
        write(
            &m.join("src/toy/rules.py.meta"),
            r#"{"exit_code_by_key": {"toy.rules.x_clamp__mutmut_2": 0, "toy.rules.x_clamp__mutmut_1": 1}}"#,
        );
        let run = load(dir.path()).unwrap();
        let mutants = &run.by_source["src/toy/rules.py"];
        assert_eq!(mutants.len(), 2);
        assert_eq!(mutants[0].key, "toy.rules.x_clamp__mutmut_1");
        assert_eq!(mutants[0].function, "toy.rules.x_clamp");
        assert_eq!(run.tests_by_function["toy.rules.x_clamp"].len(), 2);
    }

    #[test]
    fn a_missing_run_says_to_run_mutmut() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path()).err().unwrap().to_string();
        assert!(err.contains("mutmut run"), "{err}");
    }
}
