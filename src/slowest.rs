//! Where a suite's time goes, from a pytest JUnit report.

use serde::Serialize;
use std::collections::HashMap;

/// One slow test.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlowTest {
    /// The test's JUnit `classname::name`.
    pub test: String,
    /// Seconds it took, setup and teardown included.
    pub seconds: f64,
    /// Its share of the suite's total time, 0 to 100.
    pub percent: f64,
}

/// Time spent in one module.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlowModule {
    /// The module's dotted path, e.g. `tests.test_api.test_companies`.
    pub module: String,
    /// Total seconds of its tests.
    pub seconds: f64,
    /// How many tests it has.
    pub tests: usize,
    /// Its share of the suite's total time, 0 to 100.
    pub percent: f64,
}

/// A report on where the time goes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slowest {
    /// Seconds the timed tests take altogether.
    pub total_seconds: f64,
    /// How many tests had a time.
    pub tests: usize,
    /// The slowest tests, slowest first.
    pub top: Vec<SlowTest>,
    /// Share of the total the `top` tests account for, 0 to 100.
    pub top_percent: f64,
    /// Fewest tests that make up half the total time.
    pub tests_for_half: usize,
    /// The slowest modules, slowest first.
    pub modules: Vec<SlowModule>,
}

/// The module part of a JUnit `classname`: the leading dotted parts, stopping
/// at the first one that starts with an uppercase letter (a test class).
#[must_use]
pub fn module_of(classname: &str) -> String {
    classname
        .split('.')
        .take_while(|p| !p.chars().next().is_some_and(char::is_uppercase))
        .collect::<Vec<_>>()
        .join(".")
}

/// Builds the report from `seconds_by_test` (`classname::name` to seconds).
#[must_use]
pub fn report(seconds_by_test: &HashMap<String, f64>, top_n: usize) -> Slowest {
    let total: f64 = seconds_by_test.values().sum();
    let share = |s: f64| if total > 0.0 { 100.0 * s / total } else { 0.0 };
    let mut tests: Vec<(&String, f64)> = seconds_by_test.iter().map(|(k, v)| (k, *v)).collect();
    tests.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(b.0))
    });

    let top: Vec<SlowTest> = tests
        .iter()
        .take(top_n)
        .map(|(k, s)| SlowTest {
            test: (*k).clone(),
            seconds: *s,
            percent: share(*s),
        })
        .collect();
    let top_percent = top.iter().map(|t| t.percent).sum();

    let mut running = 0.0;
    let mut tests_for_half = 0;
    for (_, s) in &tests {
        if running >= total / 2.0 {
            break;
        }
        running += s;
        tests_for_half += 1;
    }

    let mut by_module: HashMap<String, (f64, usize)> = HashMap::new();
    for (k, s) in &tests {
        let classname = k.split("::").next().unwrap_or("");
        let entry = by_module.entry(module_of(classname)).or_default();
        entry.0 += s;
        entry.1 += 1;
    }
    let mut modules: Vec<SlowModule> = by_module
        .into_iter()
        .map(|(module, (seconds, tests))| SlowModule {
            module,
            seconds,
            tests,
            percent: share(seconds),
        })
        .collect();
    modules.sort_by(|a, b| {
        b.seconds
            .partial_cmp(&a.seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.module.cmp(&b.module))
    });
    modules.truncate(top_n);

    Slowest {
        total_seconds: total,
        tests: tests.len(),
        top,
        top_percent,
        tests_for_half,
        modules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(rows: &[(&str, f64)]) -> HashMap<String, f64> {
        rows.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn a_module_name_stops_at_the_test_class() {
        assert_eq!(
            module_of("tests.test_api.test_companies"),
            "tests.test_api.test_companies"
        );
        assert_eq!(module_of("tests.test_a.TestB"), "tests.test_a");
        assert_eq!(module_of("tests.test_a.TestB.Inner"), "tests.test_a");
    }

    #[test]
    fn the_report_ranks_tests_and_modules_by_time() {
        let r = report(
            &times(&[
                ("tests.test_a.TestX::slow", 8.0),
                ("tests.test_a::mid", 1.0),
                ("tests.test_b::fast", 0.5),
                ("tests.test_b::fast2", 0.5),
            ]),
            2,
        );
        assert_eq!(r.total_seconds, 10.0);
        assert_eq!(r.tests, 4);
        assert_eq!(r.top.len(), 2);
        assert_eq!(r.top[0].test, "tests.test_a.TestX::slow");
        assert_eq!(r.top[0].percent, 80.0);
        assert_eq!(r.top_percent, 90.0);
        assert_eq!(r.tests_for_half, 1);
        assert_eq!(r.modules[0].module, "tests.test_a");
        assert_eq!(r.modules[0].tests, 2);
        assert_eq!(r.modules[0].seconds, 9.0);
    }

    #[test]
    fn an_empty_report_has_no_time() {
        let r = report(&HashMap::new(), 5);
        assert_eq!(r.total_seconds, 0.0);
        assert!(r.top.is_empty() && r.modules.is_empty());
        assert_eq!(r.tests_for_half, 0);
    }
}
