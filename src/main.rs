use clap::{Parser, Subcommand};
use dupdup::config::Config;
use dupdup::static_lint::rules::Severity;
use dupdup::{baseline, dynamic, git, mutate, output, redundancy, static_lint};
use output::AnalyzeReport;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `--operators` choices for `gate`.
#[derive(Clone, Copy, clap::ValueEnum)]
enum OperatorsArg {
    Auto,
    Builtin,
    Mutmut,
}

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
        /// Report only violations not listed in this baseline file (written
        /// with --write-baseline). Existing debt doesn't fail the check; new
        /// debt does. The line number isn't part of the match.
        #[arg(long, conflicts_with = "write_baseline")]
        baseline: Option<PathBuf>,
        /// Write the current violations to this file as the baseline, and
        /// exit 0.
        #[arg(long)]
        write_baseline: Option<PathBuf>,
        /// After the violations, print a merged `parametrize` for each
        /// CLONE_TEST group, ready to paste (text output only).
        #[arg(long, conflicts_with = "json")]
        suggest: bool,
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
        #[arg(required_unless_present = "from_mutmut")]
        sources: Vec<PathBuf>,
        /// Output of `coverage json --show-contexts` from the run whose
        /// tests should be considered as candidates.
        #[arg(long, required_unless_present = "from_mutmut")]
        coverage_json: Option<PathBuf>,
        /// Attribute kills for the mutants mutmut 3 already generated in this
        /// project directory (its `mutants/` folder, after `mutmut run`),
        /// instead of generating mutants. Each mutant is selected with
        /// MUTANT_UNDER_TEST and run against the tests mutmut says cover its
        /// function, so dupdup needs no mutation operators of its own and no
        /// coverage report. --pytest-cmd runs inside that `mutants/` folder.
        #[arg(long, conflicts_with_all = ["sources", "coverage_json", "extreme"])]
        from_mutmut: Option<PathBuf>,
        /// Run mutants by forking this many workers from one pytest process
        /// (a bundled plugin; needs Python 3.11+ and fork) instead of one
        /// process per mutant. The project is imported once, so this is far
        /// faster when test startup is slow. 0 (default) keeps one process
        /// per mutant.
        #[arg(long, default_value_t = 0, conflicts_with = "from_mutmut")]
        workers: usize,
        /// With --from-mutmut, how many mutants to run at once.
        #[arg(long, default_value_t = 4, requires = "from_mutmut")]
        jobs: usize,
        /// Base command to invoke pytest with; one or more node ids are
        /// appended per invocation. No flags of dupdup's own are added.
        /// Split on whitespace with no quoting — a path or argument with a
        /// space in it doesn't work here.
        #[arg(long, default_value = "python -m pytest")]
        pytest_cmd: String,
        /// Per-invocation wall-clock budget, in seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Extreme mutation: one mutant per function, its whole body
        /// replaced by a trivial value (`pass`, `None`, `0`, `""`, `[]`...).
        /// Far fewer mutants than the point mutations, one per function, so
        /// a kill matrix covers a whole package.
        #[arg(long)]
        extreme: bool,
        /// Write the kill matrix to this file after every source file
        /// instead of printing it at the end, so a long run can be
        /// inspected while it goes and resumed with --resume.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Continue from the matrix already in --out: source files it lists
        /// as mutated are skipped and its results are kept.
        #[arg(long, requires = "out")]
        resume: bool,
        /// When a failing batch can't be attributed from its JUnit report,
        /// each candidate test is re-run alone; above this many candidates
        /// the mutant is left out as inconclusive instead.
        #[arg(long, default_value_t = 100)]
        max_drilldown: usize,
    },
    /// List the tests this change adds or edits compared with a base
    /// revision, one pytest node id per line (`pytest $(dupdup changed-tests
    /// --base origin/main)` runs just those). Cases of a parametrized test are
    /// selected together.
    ChangedTests {
        /// The revision to compare with: a branch, tag or SHA, e.g. origin/main
        #[arg(long)]
        base: String,
        /// Emit JSON (node id and new/changed) instead of plain ids
        #[arg(long)]
        json: bool,
        /// Path to config file (its `exclude` globs apply)
        #[arg(long, default_value = "dupdup.toml")]
        config: PathBuf,
    },
    /// Check a change's new and edited tests: does each catch anything the
    /// suite does not already catch? Runs only those tests under coverage,
    /// mutates only the functions they cover, and runs each mutant against
    /// them plus a few existing tests on the same lines. Cost scales with the
    /// diff; --budget-secs caps it.
    Gate {
        /// The revision the change is compared with, e.g. origin/main
        #[arg(long)]
        base: String,
        /// pytest command, starting with a Python interpreter, e.g.
        /// ".venv/bin/python -m pytest -q -p no:cacheprovider -o addopts="
        #[arg(long, default_value = "python -m pytest -q -p no:cacheprovider")]
        pytest_cmd: String,
        /// Package coverage measures (repeatable), e.g. --cov mypkg
        #[arg(long = "cov", required = true)]
        cov: Vec<String>,
        /// Per-test coverage of the whole suite on the default branch
        /// (`coverage json --show-contexts`), to find existing tests that
        /// cover the same lines. Without it a test can be shown to kill
        /// nothing but not to duplicate another.
        #[arg(long)]
        baseline_coverage: Option<PathBuf>,
        /// Stop starting new mutants after this many seconds
        #[arg(long, default_value_t = 60)]
        budget_secs: u64,
        /// Most existing tests per mutant to run beside the new ones
        #[arg(long, default_value_t = 80)]
        max_existing: usize,
        /// Per-invocation pytest timeout, in seconds
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Statement-level mutants: mutmut's operators (needs mutmut importable
        /// in the --pytest-cmd interpreter), dupdup's built-in ones, or auto
        /// (mutmut's if available)
        #[arg(long, value_enum, default_value_t = OperatorsArg::Auto)]
        operators: OperatorsArg,
        /// Fork this many mutant workers per pytest process (needs Python
        /// 3.11+ and fork)
        #[arg(long, default_value_t = 4)]
        workers: usize,
        /// Run one pytest process per mutant instead of forking workers
        #[arg(long)]
        no_fork: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
        /// Exit 1 if any touched test killed none of the mutants it ran against
        #[arg(long)]
        strict: bool,
        /// Path to config file (its `exclude` globs apply)
        #[arg(long, default_value = "dupdup.toml")]
        config: PathBuf,
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
            baseline,
            write_baseline,
            suggest,
        } => run_check(&CheckOptions {
            paths: &paths,
            staged,
            all,
            json,
            config: &config,
            baseline: baseline.as_deref(),
            write_baseline: write_baseline.as_deref(),
            suggest,
        }),
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
            extreme,
            out,
            resume,
            max_drilldown,
            from_mutmut,
            jobs,
            workers,
        } => run_mutate(&MutateOptions {
            workers,
            sources: &sources,
            coverage_json: coverage_json.as_deref(),
            from_mutmut: from_mutmut.as_deref(),
            jobs,
            pytest_cmd: &pytest_cmd,
            timeout_secs,
            extreme,
            out: out.as_deref(),
            resume,
            max_drilldown,
        }),
        Commands::ChangedTests { base, json, config } => run_changed_tests(&base, json, &config),
        Commands::Gate {
            base,
            pytest_cmd,
            cov,
            baseline_coverage,
            budget_secs,
            max_existing,
            timeout_secs,
            operators,
            workers,
            no_fork,
            json,
            strict,
            config,
        } => run_gate(&GateArgs {
            operators,
            workers,
            no_fork,
            base: &base,
            pytest_cmd: &pytest_cmd,
            cov,
            baseline_coverage: baseline_coverage.as_deref(),
            budget_secs,
            max_existing,
            timeout_secs,
            json,
            strict,
            config: &config,
        }),
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

/// Everything `dupdup gate` was asked to do.
struct GateArgs<'a> {
    operators: OperatorsArg,
    workers: usize,
    no_fork: bool,
    base: &'a str,
    pytest_cmd: &'a str,
    cov: Vec<String>,
    baseline_coverage: Option<&'a Path>,
    budget_secs: u64,
    max_existing: usize,
    timeout_secs: u64,
    json: bool,
    strict: bool,
    config: &'a Path,
}

fn run_gate(args: &GateArgs) -> anyhow::Result<()> {
    use dupdup::gate::{self, Verdict};
    let config = Config::load(args.config)?;
    let exclude = compile_globs(&config.exclude)?;
    let root = git::repo_root(&std::env::current_dir()?)?;
    let files: Vec<PathBuf> = git::all_files(&root)?
        .into_iter()
        .filter(|p| is_python_test_file(p) && !is_excluded(p, &exclude))
        .collect();
    let touched = dupdup::changed::touched_tests(&root, args.base, &files)?;
    eprintln!(
        "{} test(s) added or changed since {}",
        touched.len(),
        args.base
    );
    let opts = gate::GateOptions {
        root: &root,
        pytest_cmd: args
            .pytest_cmd
            .split_whitespace()
            .map(String::from)
            .collect(),
        cov_sources: args.cov.clone(),
        baseline_coverage: args.baseline_coverage,
        budget: Duration::from_secs(args.budget_secs),
        max_existing: args.max_existing,
        timeout: Duration::from_secs(args.timeout_secs),
        workers: if args.no_fork { 0 } else { args.workers },
        operators: match args.operators {
            OperatorsArg::Auto => gate::Operators::Auto,
            OperatorsArg::Builtin => gate::Operators::Builtin,
            OperatorsArg::Mutmut => gate::Operators::Mutmut,
        },
    };
    let verdicts = gate::run(&opts, &touched)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&verdicts)?);
    } else {
        for v in &verdicts {
            match &v.verdict {
                Verdict::Unique { mutants } => println!(
                    "UNIQUE       {}  (kills {} mutant(s) no existing test kills)",
                    v.node_id,
                    mutants.len()
                ),
                Verdict::Overlap { with } => println!(
                    "OVERLAP      {}  (kills only what {} also kills)",
                    v.node_id,
                    with.join(", ")
                ),
                Verdict::KillsNothing { evaluated } => println!(
                    "KILLS_NOTHING {}  (0 of {evaluated} mutants killed)",
                    v.node_id
                ),
                Verdict::NoEvidence => println!(
                    "NO_EVIDENCE  {}  (no mutated function covered, or out of time)",
                    v.node_id
                ),
            }
        }
    }
    let nothing = verdicts
        .iter()
        .filter(|v| matches!(v.verdict, Verdict::KillsNothing { .. }))
        .count();
    if args.strict && nothing > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn run_changed_tests(base: &str, json: bool, config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    let exclude = compile_globs(&config.exclude)?;
    let root = git::repo_root(&std::env::current_dir()?)?;
    let files: Vec<PathBuf> = git::all_files(&root)?
        .into_iter()
        .filter(|p| is_python_test_file(p) && !is_excluded(p, &exclude))
        .collect();
    let touched = dupdup::changed::touched_tests(&root, base, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&touched)?);
    } else {
        for t in &touched {
            println!("{}", t.node_id);
        }
    }
    Ok(())
}

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

/// Everything `dupdup check` was asked to do.
struct CheckOptions<'a> {
    paths: &'a [PathBuf],
    staged: bool,
    all: bool,
    json: bool,
    config: &'a Path,
    baseline: Option<&'a Path>,
    write_baseline: Option<&'a Path>,
    suggest: bool,
}

fn run_check(opts: &CheckOptions) -> anyhow::Result<()> {
    let CheckOptions {
        paths,
        staged,
        all,
        json,
        config: config_path,
        baseline: baseline_path,
        write_baseline: write_baseline_path,
        suggest,
    } = *opts;
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
    let mut suggestions: Vec<(String, static_lint::clones::Suggestion)> = Vec::new();

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
            let file = rel_path.to_string_lossy().into_owned();
            violations.extend(static_lint::check_file_with_helpers(
                &file, &source, &config, &helpers,
            )?);
            if suggest {
                suggestions.extend(
                    static_lint::clones::suggestions(&source)?
                        .into_iter()
                        .map(|s| (file.clone(), s)),
                );
            }
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
            let file = path.to_string_lossy().into_owned();
            violations.extend(static_lint::check_file_with_helpers(
                &file, source, &config, &helpers,
            )?);
            if suggest {
                suggestions.extend(
                    static_lint::clones::suggestions(source)?
                        .into_iter()
                        .map(|s| (file.clone(), s)),
                );
            }
        }
    }

    if let Some(path) = write_baseline_path {
        let n = baseline::write(path, &violations)?;
        eprintln!(
            "wrote {n} baseline entr{} to {}",
            if n == 1 { "y" } else { "ies" },
            path.display()
        );
        return Ok(());
    }
    if let Some(path) = baseline_path {
        let filtered = baseline::filter(violations, &baseline::load(path)?);
        eprintln!(
            "{} new violation(s); {} known in the baseline, {} baseline entr{} no longer occur{}",
            filtered.new.len(),
            filtered.known,
            filtered.fixed,
            if filtered.fixed == 1 { "y" } else { "ies" },
            if filtered.fixed == 1 { "s" } else { "" }
        );
        violations = filtered.new;
    }

    if json {
        output::print_check_json(&violations)?;
    } else {
        output::print_check_human(&violations);
        // Only for groups still reported (not suppressed, not in the baseline).
        for (file, s) in &suggestions {
            if violations
                .iter()
                .any(|v| v.rule == "CLONE_TEST" && v.file == *file && v.line == s.line)
            {
                println!();
                println!("suggestion for {file}:{}:", s.line);
                println!("{}", s.text);
            }
        }
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

/// Makes a kill-matrix finding relative to what was mutated. Prints the scope
/// and, when `coverage_files` says which files each test covered, sets aside
/// `COVERS_NOTHING` findings for tests that also cover code outside the
/// mutated files: those tests may be valuable for that other code, which no
/// mutant touched. Returns the set-aside test ids, which are also kept out of
/// the removal set.
fn set_aside_outside_scope(
    findings: &mut Vec<redundancy::RedundancyFinding>,
    mutated: &HashSet<&str>,
    coverage_files: Option<&HashMap<String, HashSet<String>>>,
    mutated_files: usize,
) -> HashSet<String> {
    if mutated_files == 0 {
        eprintln!(
            "note: this kill matrix doesn't record which files were mutated, so its findings can't \
             be tied to a scope; re-run `dupdup mutate` to record it"
        );
        return HashSet::new();
    }
    eprintln!(
        "mutation scope: {mutated_files} source file(s). COVERS_NOTHING and --removal-set are \
         relative to them, not to the whole codebase."
    );
    let Some(files) = coverage_files else {
        eprintln!(
            "note: pass --coverage-json or --coverage-data as well, so tests that also cover code \
             outside the mutated files can be set aside"
        );
        return HashSet::new();
    };
    let outside: HashSet<String> = files
        .iter()
        .filter(|(_, covered)| covered.iter().any(|f| !mutated.contains(f.as_str())))
        .map(|(id, _)| id.clone())
        .collect();
    let before = findings.len();
    findings.retain(|f| {
        !(f.kind == redundancy::RedundancyKind::CoversNothing && outside.contains(&f.test_id))
    });
    let set_aside = before - findings.len();
    if set_aside > 0 {
        eprintln!(
            "{set_aside} COVERS_NOTHING finding(s) set aside: those tests also cover code outside \
             the mutated files"
        );
    }
    outside
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

    let mut coverage_files: Option<HashMap<String, HashSet<String>>> = None;
    let loaded_coverage = match (coverage_json, coverage_data) {
        (Some(p), _) => Some(dynamic::coverage::load(p, include_fixtures)?),
        (None, Some(p)) => Some(dynamic::coverage::load_data(p, include_fixtures)?),
        (None, None) => None,
    };
    if let Some(l) = &loaded_coverage {
        coverage_files = Some(l.files_by_test.clone());
    }
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
                            evaluated: None,
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
                        evaluated: None,
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
        for f in &mut findings {
            if f.kind == redundancy::RedundancyKind::CoversNothing {
                f.evaluated = loaded.evaluated.get(&f.test_id).copied();
            }
        }
        let mutated: HashSet<&str> = loaded.mutated.keys().map(String::as_str).collect();
        let outside_scope = set_aside_outside_scope(
            &mut findings,
            &mutated,
            coverage_files.as_ref(),
            loaded.mutated.len(),
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
                    .filter(|id| !outside_scope.contains(id))
                    .map(|test_id| redundancy::RedundancyFinding {
                        kind: redundancy::RedundancyKind::Removable,
                        test_id,
                        related: Vec::new(),
                        other_matches: 0,
                        evaluated: None,
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

/// Everything `dupdup mutate` was asked to do.
struct MutateOptions<'a> {
    workers: usize,
    sources: &'a [PathBuf],
    coverage_json: Option<&'a Path>,
    from_mutmut: Option<&'a Path>,
    jobs: usize,
    pytest_cmd: &'a str,
    timeout_secs: u64,
    extreme: bool,
    out: Option<&'a Path>,
    resume: bool,
    max_drilldown: usize,
}

fn run_mutate(opts: &MutateOptions) -> anyhow::Result<()> {
    let pytest_cmd: Vec<String> = opts
        .pytest_cmd
        .split_whitespace()
        .map(String::from)
        .collect();
    anyhow::ensure!(!pytest_cmd.is_empty(), "--pytest-cmd must not be empty");

    let cfg = mutate::RunConfig {
        pytest_cmd,
        timeout: Duration::from_secs(opts.timeout_secs),
        extreme: opts.extreme,
        max_drilldown: opts.max_drilldown,
        env: Vec::new(),
        cwd: None,
        deadline: None,
        narrow_candidates: None,
        workers: opts.workers,
        generate: None,
    };
    let resume = match (opts.resume, opts.out) {
        (true, Some(path)) if path.exists() => {
            Some(serde_json::from_str(&std::fs::read_to_string(path)?)?)
        }
        _ => None,
    };
    let mut progress = |m: &mutate::KillMatrix| -> anyhow::Result<()> {
        match opts.out {
            Some(path) => output::write_kill_matrix_json(path, m),
            None => Ok(()),
        }
    };
    let matrix = if let Some(root) = opts.from_mutmut {
        let run = mutate::mutmut::load(root)?;
        mutate::mutmut::collect(root, &run, &cfg, opts.jobs, resume, &mut progress)?
    } else {
        let coverage_json = opts
            .coverage_json
            .ok_or_else(|| anyhow::anyhow!("--coverage-json is required without --from-mutmut"))?;
        let cwd = std::env::current_dir()?;
        let repo_root = git::repo_root(&cwd)?;
        let coverage = dynamic::coverage::load_line_coverage(coverage_json)?;
        mutate::collect(
            &repo_root,
            opts.sources,
            &coverage,
            &cfg,
            resume,
            &mut progress,
        )?
    };
    if opts.out.is_none() {
        output::print_kill_matrix_json(&matrix)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing(id: &str) -> redundancy::RedundancyFinding {
        redundancy::RedundancyFinding {
            kind: redundancy::RedundancyKind::CoversNothing,
            test_id: id.into(),
            related: vec![],
            other_matches: 0,
            evaluated: Some(1),
        }
    }

    fn files(pairs: &[(&str, &[&str])]) -> HashMap<String, HashSet<String>> {
        pairs
            .iter()
            .map(|(id, fs)| {
                (
                    (*id).to_string(),
                    fs.iter().map(|f| (*f).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_test_that_also_covers_unmutated_code_is_set_aside() {
        let mut findings = vec![nothing("t.py::inside"), nothing("t.py::outside")];
        let mutated: HashSet<&str> = ["pkg/a.py"].into_iter().collect();
        let cov = files(&[
            ("t.py::inside", &["pkg/a.py"]),
            ("t.py::outside", &["pkg/a.py", "pkg/b.py"]),
        ]);
        let aside = set_aside_outside_scope(&mut findings, &mutated, Some(&cov), 1);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].test_id, "t.py::inside");
        assert!(aside.contains("t.py::outside") && !aside.contains("t.py::inside"));
    }

    #[test]
    fn without_coverage_or_scope_nothing_is_set_aside() {
        let mutated: HashSet<&str> = ["pkg/a.py"].into_iter().collect();
        let mut findings = vec![nothing("t.py::x")];
        assert!(set_aside_outside_scope(&mut findings, &mutated, None, 1).is_empty());
        assert_eq!(findings.len(), 1);
        let cov = files(&[("t.py::x", &["pkg/zzz.py"])]);
        let mut findings = vec![nothing("t.py::x")];
        assert!(set_aside_outside_scope(&mut findings, &HashSet::new(), Some(&cov), 0).is_empty());
        assert_eq!(findings.len(), 1);
    }

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
