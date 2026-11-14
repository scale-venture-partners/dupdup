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
            junit_xml,
            allow_incomplete_context,
            include_fixtures,
            kill_matrix_json,
            json,
        } => run_analyze(
            coverage_json.as_deref(),
            junit_xml.as_deref(),
            allow_incomplete_context,
            include_fixtures,
            kill_matrix_json.as_deref(),
            json,
        ),
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
        // Helpers can live in any tracked Python file (conftest.py, a shared
        // module), not only test files, so index every one of them.
        let index_sources: Vec<String> = git::all_files(&root)?
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "py"))
            .filter_map(|p| std::fs::read_to_string(root.join(p)).ok())
            .collect();
        let helpers = static_lint::AssertingHelpers::build(
            index_sources.iter().map(String::as_str),
            &config.no_assertions.helpers,
        );
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

fn run_analyze(
    coverage_json: Option<&Path>,
    junit_xml: Option<&Path>,
    allow_incomplete_context: bool,
    include_fixtures: bool,
    kill_matrix_json: Option<&Path>,
    json: bool,
) -> anyhow::Result<()> {
    let mut reports = Vec::new();

    if let Some(cov_path) = coverage_json {
        let loaded = dynamic::coverage::load(cov_path, include_fixtures)?;
        let mut findings = redundancy::analyze(&loaded.tests);
        match junit_xml {
            Some(junit_path) => {
                let summary = dynamic::junit::load(junit_path)?;
                check_context_completeness(
                    loaded.tests_with_context,
                    &summary,
                    allow_incomplete_context,
                )?;
                let analyzed = loaded.tests.iter().map(|t| t.test_id.as_str());
                findings.extend(
                    dynamic::junit::tests_without_coverage(&summary, analyzed)
                        .into_iter()
                        .map(|test_id| redundancy::RedundancyFinding {
                            kind: redundancy::RedundancyKind::NoCoverage,
                            test_id,
                            related: Vec::new(),
                        }),
                );
            }
            None => eprintln!(
                "warning: no --junit-xml given — a run with tests missing context data \
                 can't be detected. Pass --junit-xml to enable that check."
            ),
        }
        reports.push(AnalyzeReport {
            source: "coverage",
            findings,
        });
    }

    if let Some(km_path) = kill_matrix_json {
        let loaded = dynamic::killmatrix::load(km_path)?;
        reports.push(AnalyzeReport {
            source: "mutation",
            findings: redundancy::analyze(&loaded.tests),
        });
    }

    if reports.is_empty() {
        anyhow::bail!("nothing to analyze: pass --coverage-json and/or --kill-matrix-json");
    }

    if json {
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
