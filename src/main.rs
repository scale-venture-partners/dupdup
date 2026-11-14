use clap::{Parser, Subcommand};
use dupdup::config::Config;
use dupdup::static_lint::rules::Severity;
use dupdup::{dynamic, git, mutate, output, redundancy, static_lint};
use output::AnalyzeReport;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "dupdup",
    version,
    about = "Finds duplicative, redundant, and useless tests"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Static AST rules over test source — no execution, pre-commit friendly
    Check {
        /// Explicit files to check (bypasses git discovery entirely)
        paths: Vec<PathBuf>,
        /// Check files staged for commit
        #[arg(long)]
        staged: bool,
        /// Check every git-tracked file in the repo
        #[arg(long)]
        all: bool,
        /// Emit machine-readable JSON instead of human-readable text
        #[arg(long)]
        json: bool,
        /// Path to config file
        #[arg(long, default_value = "dupdup.toml")]
        config: PathBuf,
    },
    /// Redundancy analysis over a coverage run and/or a mutation kill-matrix.
    /// Requires executing the test suite first — this is not a linter.
    Analyze {
        /// Output of `coverage json --show-contexts` after a
        /// `pytest --cov=<pkg> --cov-context=test` run
        #[arg(long)]
        coverage_json: Option<PathBuf>,
        /// coverage.py's own data file (`.coverage`) from a
        /// `pytest --cov=<pkg> --cov-branch --cov-context=test` run. Unlike
        /// --coverage-json it keeps branch arcs, so two tests that run the
        /// same lines but take different branches stay different.
        #[arg(long, conflicts_with = "coverage_json")]
        coverage_data: Option<PathBuf>,
        /// pytest `--junitxml` output from the same run as --coverage-json.
        /// Cross-checked against the test count coverage-json actually
        /// recorded contexts for, so a run missing context data for tests
        /// that genuinely ran fails loudly instead of analyzing partial
        /// data. A shortfall is often benign on a large real suite (tests
        /// that touch no traceable line), not a broken coverage run — pass
        /// --allow-incomplete-context once you've confirmed that's the case.
        #[arg(long)]
        junit_xml: Option<PathBuf>,
        /// Proceed even if --junit-xml finds tests with no coverage-context
        /// data, instead of erroring. Use once you've confirmed the gap is
        /// benign (see the error message this would otherwise print).
        #[arg(long)]
        allow_incomplete_context: bool,
        /// Count lines run by a test's fixtures (`setup`/`teardown`
        /// phases) as that test's coverage. Off by default: fixture lines
        /// aren't exercised by the test body, and a shared fixture makes
        /// unrelated tests look like subsets of each other.
        #[arg(long)]
        include_fixtures: bool,
        /// Also report a jointly coverage-preserving set of tests
        /// (`REMOVABLE`) that can all be removed at once without shrinking
        /// any file's coverage. Preserves coverage, not behaviour: two tests
        /// can cover the same lines and check different things.
        #[arg(long)]
        removal_set: bool,
        /// Also report CONTRIBUTES_NOTHING_UNIQUE: a test whose every
        /// requirement is covered by a few other tests combined (named in
        /// the finding). Off by default: each requirement can be covered by
        /// a different peer, so this rarely means the test's scenario is
        /// covered.
        #[arg(long)]
        union_relation: bool,
        /// Report every relation from --kill-matrix-json even when it has
        /// fewer killed mutants than half the number of tests. By default a
        /// matrix that sparse reports only COVERS_NOTHING, since the other
        /// relations can't tell tests apart over so few mutants.
        #[arg(long)]
        allow_sparse_kill_matrix: bool,
        /// Directory the coverage run's test paths are relative to (the
        /// pytest rootdir), inside a git repo. Default: the current directory.
        #[arg(long, default_value = ".")]
        test_root: PathBuf,
        /// Skip reading each test's source. By default two tests only count
        /// as redundant if they also make the same assertions; without
        /// this, line coverage alone decides, which flags tests that run the
        /// same code to check different things.
        #[arg(long)]
        no_assertion_check: bool,
        /// Test-to-killed-mutant JSON (see `dupdup::dynamic::killmatrix` for
        /// the schema). No upstream tool produces this yet.
        #[arg(long)]
        kill_matrix_json: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Self-contained mutation-testing kill-matrix collector. Mutates each
    /// source in place, one mutant at a time, running only the tests
    /// --coverage-json says cover it. Prints the kill-matrix JSON `analyze
    /// --kill-matrix-json` reads.
    Mutate {
        /// Source file(s) to mutate — application code, not test files.
        /// Spelled exactly as they appear as keys in --coverage-json's own
        /// "files" map, and run from the same working directory that
        /// coverage run used.
        #[arg(required = true)]
        sources: Vec<PathBuf>,
        /// Output of `coverage json --show-contexts` from the run whose
        /// tests should be considered as candidates.
        #[arg(long)]
        coverage_json: PathBuf,
        /// Base command to invoke pytest with; one or more node ids are
        /// appended per invocation. No flags of dupdup's own are added.
        /// Split on whitespace with no quoting — a path or argument with a
        /// space in it doesn't work here.
        #[arg(long, default_value = "python -m pytest")]
        pytest_cmd: String,
        /// Per-invocation wall-clock budget, in seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    /// Write a default dupdup.toml to the current directory
    Init,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Check {
            paths,
            staged,
            all,
            json,
            config,
        } => run_check(&paths, staged, all, json, &config),
        Commands::Analyze {
            coverage_json,
            coverage_data,
            junit_xml,
            allow_incomplete_context,
            include_fixtures,
            removal_set,
            union_relation,
            allow_sparse_kill_matrix,
            test_root,
            no_assertion_check,
            kill_matrix_json,
            json,
        } => run_analyze(&AnalyzeOptions {
            coverage_json: coverage_json.as_deref(),
            coverage_data: coverage_data.as_deref(),
            junit_xml: junit_xml.as_deref(),
            allow_incomplete_context,
            include_fixtures,
            removal_set,
            union_relation,
            allow_sparse_kill_matrix,
            test_root: &test_root,
            no_assertion_check,
            kill_matrix_json: kill_matrix_json.as_deref(),
            json,
        }),
        Commands::Mutate {
            sources,
            coverage_json,
            pytest_cmd,
            timeout_secs,
        } => run_mutate(&sources, &coverage_json, &pytest_cmd, timeout_secs),
        Commands::Init => run_init(),
    }
}

const DEFAULT_CONFIG_TEMPLATE: &str = r"[dupdup]
# Glob patterns (relative to repo root) to skip entirely.
exclude = []

[dupdup.no_assertions]
enabled = true

[dupdup.duplicate_assert]
enabled = true

[dupdup.tautological_assert]
enabled = true
";

fn run_init() -> anyhow::Result<()> {
    let path = PathBuf::from("dupdup.toml");
    if path.exists() {
        anyhow::bail!("dupdup.toml already exists");
    }
    std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn is_python_test_file(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => {
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        }
        None => false,
    }
}

fn glob_to_regex(glob: &str) -> String {
    let mut out = String::from("^");
    for c in glob.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c if r"\.+()|[]{}^$".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    out
}

fn compile_globs(patterns: &[String]) -> anyhow::Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| Regex::new(&glob_to_regex(p)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)
}

fn is_excluded(path: &Path, patterns: &[Regex]) -> bool {
    let path_str = path.to_string_lossy();
    patterns.iter().any(|re| re.is_match(&path_str))
}

fn run_check(
    paths: &[PathBuf],
    staged: bool,
    all: bool,
    json: bool,
    config_path: &Path,
) -> anyhow::Result<()> {
    let modes = [!paths.is_empty(), staged, all]
        .iter()
        .filter(|b| **b)
        .count();
    if modes == 0 {
        anyhow::bail!("specify one of: explicit paths, --staged, or --all");
    }
    if modes > 1 {
        anyhow::bail!("--staged, --all, and explicit paths are mutually exclusive");
    }

    let config = Config::load(config_path)?;
    let exclude_patterns = compile_globs(&config.exclude)?;
    let mut violations = Vec::new();

    if paths.is_empty() {
        let cwd = std::env::current_dir()?;
        let root = git::repo_root(&cwd)?;
        let files = if staged {
            git::staged_files(&root)?
        } else {
            git::all_files(&root)?
        };
        let helpers = static_lint::helpers::index_repo(&root, &config.no_assertions.helpers)?;
        for rel_path in files {
            if !is_python_test_file(&rel_path) || is_excluded(&rel_path, &exclude_patterns) {
                continue;
            }
            let source = if staged {
                match git::staged_content(&root, &rel_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                }
            } else {
                match std::fs::read_to_string(root.join(&rel_path)) {
                    Ok(s) => s,
                    Err(_) => continue,
                }
            };
            violations.extend(static_lint::check_file_with_helpers(
                &rel_path.to_string_lossy(),
                &source,
                &config,
                &helpers,
            )?);
        }
    } else {
        // An explicit path is checked regardless of pytest's test-file naming
        // convention — the caller named it, so it's in scope. That
        // convention only gates which files --staged/--all bother to parse.
        let mut sources = Vec::new();
        for path in paths {
            if is_excluded(path, &exclude_patterns) {
                continue;
            }
            sources.push((path, std::fs::read_to_string(path)?));
        }
        let helpers = static_lint::AssertingHelpers::build(
            sources.iter().map(|(_, s)| s.as_str()),
            &config.no_assertions.helpers,
        );
        for (path, source) in &sources {
            violations.extend(static_lint::check_file_with_helpers(
                &path.to_string_lossy(),
                source,
                &config,
                &helpers,
            )?);
        }
    }

    if json {
        output::print_check_json(&violations)?;
    } else {
        output::print_check_human(&violations);
    }

    if violations.iter().any(|v| v.severity == Severity::Error) {
        std::process::exit(1);
    }
    Ok(())
}

/// `--junit-xml` gives the count of tests that actually executed their body
/// (passed + failed, excluding skipped/errored), so a shortfall against the
/// coverage-json context count means some of them have no context data at
/// all. Two real causes, both confirmed against real codebases: Python
/// 3.12+'s `sysmon` coverage core drops dynamic contexts silently (fix:
/// `COVERAGE_CORE=ctrace`), or a test genuinely executes no traceable line
/// in the covered package — a pure static-analysis check, or code whose
/// only work happens inside a compiled extension (e.g. Pydantic v2's Rust
/// validation core). The second cause is normal in a large, real codebase,
/// so this only hard-errors by default; `--allow-incomplete-context` lets a
/// caller who has confirmed the gap is benign proceed anyway.
fn check_context_completeness(
    found_test_count: usize,
    summary: &dynamic::junit::JunitSummary,
    allow_incomplete: bool,
) -> anyhow::Result<()> {
    if found_test_count < summary.ran {
        let message = format!(
            "coverage-json has dynamic-context data for {found_test_count} test(s), but \
             junit-xml recorded {ran} test(s) that ran ({skipped} skipped, {errored} errored). \
             {missing} test(s) have no context data at all — either Python's `sysmon` coverage \
             core (3.12+) silently dropped them (try `COVERAGE_CORE=ctrace`), or they genuinely \
             execute no traceable line in the covered package (e.g. a compiled-extension-only \
             code path, or a test that touches nothing under --cov). Pass \
             --allow-incomplete-context once you've confirmed which.",
            ran = summary.ran,
            skipped = summary.skipped,
            errored = summary.errored,
            missing = summary.ran.saturating_sub(found_test_count),
        );
        if allow_incomplete {
            eprintln!("warning: {message}");
        } else {
            anyhow::bail!(message);
        }
    }
    Ok(())
}

/// Folds each test's assertions into its requirement set. A test whose
/// source can't be found gains none, so that is reported, and an
/// all-unresolved run is an error: it almost always means a wrong
/// `--test-root`, and silently falling back to coverage alone would bring
/// back the noise this check exists to remove.
fn add_assertions(
    tests: &mut Vec<redundancy::TestCoverage>,
    test_root: &Path,
) -> anyhow::Result<()> {
    if tests.is_empty() {
        return Ok(());
    }
    let root = git::repo_root(test_root)?;
    let rel = test_root.canonicalize()?;
    let helpers = static_lint::helpers::index_repo(&root, &[])?;
    let fingerprints =
        dynamic::assertions::load(tests.iter().map(|t| t.test_id.as_str()), &rel, &helpers);
    if fingerprints.unresolved == tests.len() {
        anyhow::bail!(
            "none of the {} tests' source files were found under {}. Pass --test-root \
             (the pytest rootdir the coverage run used), or --no-assertion-check to \
             compare line coverage alone.",
            tests.len(),
            rel.display()
        );
    }
    if fingerprints.unresolved > 0 {
        eprintln!(
            "warning: {} of {} tests' source could not be found under {} (for example tests \
             inherited from a base class outside the repo); they are left out of the redundancy \
             relations, since line coverage alone can't tell them apart",
            fingerprints.unresolved,
            tests.len(),
            rel.display()
        );
        dynamic::assertions::drop_unresolved(tests, &fingerprints);
    }
    dynamic::assertions::augment(tests, &fingerprints);
    Ok(())
}

/// Everything `dupdup analyze` was asked to do.
struct AnalyzeOptions<'a> {
    coverage_json: Option<&'a Path>,
    coverage_data: Option<&'a Path>,
    junit_xml: Option<&'a Path>,
    allow_incomplete_context: bool,
    include_fixtures: bool,
    removal_set: bool,
    union_relation: bool,
    allow_sparse_kill_matrix: bool,
    test_root: &'a Path,
    no_assertion_check: bool,
    kill_matrix_json: Option<&'a Path>,
    json: bool,
}

fn run_analyze(opts: &AnalyzeOptions) -> anyhow::Result<()> {
    let AnalyzeOptions {
        coverage_json,
        coverage_data,
        junit_xml,
        allow_incomplete_context,
        include_fixtures,
        removal_set,
        union_relation,
        allow_sparse_kill_matrix,
        test_root,
        no_assertion_check,
        kill_matrix_json,
        json,
    } = *opts;
    let mut reports = Vec::new();

    let loaded_coverage = match (coverage_json, coverage_data) {
        (Some(p), _) => Some(dynamic::coverage::load(p, include_fixtures)?),
        (None, Some(p)) => Some(dynamic::coverage::load_data(p, include_fixtures)?),
        (None, None) => None,
    };
    if let Some(mut loaded) = loaded_coverage {
        let all_ids: Vec<String> = loaded.tests.iter().map(|t| t.test_id.clone()).collect();
        if !no_assertion_check {
            add_assertions(&mut loaded.tests, test_root)?;
        }
        let mut findings = redundancy::analyze_with(&loaded.tests, union_relation);
        match junit_xml {
            Some(junit_path) => {
                let summary = dynamic::junit::load(junit_path)?;
                check_context_completeness(
                    loaded.tests_with_context,
                    &summary,
                    allow_incomplete_context,
                )?;
                // Every test with coverage counts as covered, including ones
                // left out of the relations because their source wasn't found.
                let analyzed = all_ids.iter().map(String::as_str);
                findings.extend(
                    dynamic::junit::tests_without_coverage(&summary, analyzed)
                        .into_iter()
                        .map(|test_id| redundancy::RedundancyFinding {
                            kind: redundancy::RedundancyKind::NoCoverage,
                            test_id,
                            related: Vec::new(),
                            other_matches: 0,
                        }),
                );
            }
            None => eprintln!(
                "warning: no --junit-xml given — a run with tests missing context data \
                 can't be detected. Pass --junit-xml to enable that check."
            ),
        }
        if removal_set {
            findings.extend(
                redundancy::removable_set(&loaded.tests)
                    .into_iter()
                    .map(|test_id| redundancy::RedundancyFinding {
                        kind: redundancy::RedundancyKind::Removable,
                        test_id,
                        related: Vec::new(),
                        other_matches: 0,
                    }),
            );
        }
        reports.push(AnalyzeReport {
            source: "coverage",
            findings,
            tests_analyzed: loaded.tests.len(),
        });
    }

    if let Some(km_path) = kill_matrix_json {
        let loaded = dynamic::killmatrix::load(km_path)?;
        let mut findings =
            redundancy::analyze_scoped(&loaded.tests, true, redundancy::Scope::Anywhere);
        eprintln!(
            "mutation: {} killed mutant(s) across {} test(s)",
            redundancy::universe_size(&loaded.tests),
            loaded.tests.len()
        );
        if redundancy::is_sparse(&loaded.tests) && !allow_sparse_kill_matrix {
            eprintln!(
                "warning: too few killed mutants to tell tests apart; reporting only \
                 COVERS_NOTHING. Mutate more of the code, or pass --allow-sparse-kill-matrix."
            );
            findings.retain(|f| f.kind == redundancy::RedundancyKind::CoversNothing);
        }
        if removal_set {
            findings.extend(
                redundancy::removable_set_scoped(&loaded.tests, redundancy::Scope::Anywhere)
                    .into_iter()
                    .map(|test_id| redundancy::RedundancyFinding {
                        kind: redundancy::RedundancyKind::Removable,
                        test_id,
                        related: Vec::new(),
                        other_matches: 0,
                    }),
            );
        }
        reports.push(AnalyzeReport {
            source: "mutation",
            findings,
            tests_analyzed: loaded.tests.len(),
        });
    }

    if reports.is_empty() {
        anyhow::bail!(
            "nothing to analyze: pass --coverage-json, --coverage-data and/or --kill-matrix-json"
        );
    }

    if json {
        for report in &reports {
            if let Some(line) = report.flagged_summary() {
                eprintln!("{line}");
            }
        }
        output::print_analyze_json(&reports)?;
    } else {
        output::print_analyze_human(&reports);
    }
    Ok(())
}

fn run_mutate(
    sources: &[PathBuf],
    coverage_json: &Path,
    pytest_cmd: &str,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let pytest_cmd: Vec<String> = pytest_cmd.split_whitespace().map(String::from).collect();
    anyhow::ensure!(!pytest_cmd.is_empty(), "--pytest-cmd must not be empty");

    let cwd = std::env::current_dir()?;
    let repo_root = git::repo_root(&cwd)?;
    let coverage = dynamic::coverage::load_line_coverage(coverage_json)?;
    let cfg = mutate::RunConfig {
        pytest_cmd,
        timeout: Duration::from_secs(timeout_secs),
    };
    let kills = mutate::collect(&repo_root, sources, &coverage, &cfg)?;
    output::print_kill_matrix_json(&kills)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_python_test_file_matches_pytest_naming_convention() {
        assert!(is_python_test_file(Path::new("test_foo.py")));
        assert!(is_python_test_file(Path::new("foo_test.py")));
        assert!(is_python_test_file(Path::new("dir/test_foo.py")));
        assert!(!is_python_test_file(Path::new("foo.py")));
        assert!(!is_python_test_file(Path::new("test_foo.txt")));
        assert!(!is_python_test_file(Path::new("conftest.py")));
    }

    #[test]
    fn glob_to_regex_translates_star_and_escapes_dots() {
        assert_eq!(glob_to_regex("vendor/*"), r"^vendor/.*$");
        assert_eq!(glob_to_regex("a.b"), r"^a\.b$");
    }

    #[test]
    fn is_excluded_matches_a_compiled_glob() {
        let patterns = compile_globs(&["vendor/*".to_string()]).unwrap();
        assert!(is_excluded(Path::new("vendor/thing.py"), &patterns));
        assert!(!is_excluded(Path::new("src/thing.py"), &patterns));
    }

    #[test]
    fn is_excluded_with_no_patterns_excludes_nothing() {
        let patterns = compile_globs(&[]).unwrap();
        assert!(!is_excluded(Path::new("anything.py"), &patterns));
    }

    fn write_junit(xml: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, xml.as_bytes()).unwrap();
        f
    }

    const TWO_PASSED_JUNIT: &str = r#"<testsuite tests="2">
        <testcase classname="t" name="a"/>
        <testcase classname="t" name="b"/>
    </testsuite>"#;

    #[test]
    fn context_shortfall_errors_by_default() {
        let s = dynamic::junit::load(write_junit(TWO_PASSED_JUNIT).path()).unwrap();
        assert!(check_context_completeness(1, &s, false).is_err());
    }

    #[test]
    fn context_shortfall_is_a_warning_not_an_error_when_allowed() {
        let s = dynamic::junit::load(write_junit(TWO_PASSED_JUNIT).path()).unwrap();
        assert!(check_context_completeness(1, &s, true).is_ok());
    }

    #[test]
    fn no_shortfall_is_always_ok() {
        let s = dynamic::junit::load(write_junit(TWO_PASSED_JUNIT).path()).unwrap();
        assert!(check_context_completeness(2, &s, false).is_ok());
    }
}
