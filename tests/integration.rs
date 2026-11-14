use dupdup::config::Config;
use dupdup::static_lint::check_file;

fn violations_for(fixture: &str) -> Vec<dupdup::static_lint::rules::Violation> {
    let config = Config::default();
    check_file("fixture", fixture, &config).expect("parse should succeed")
}

#[test]
fn flags_no_assertions_duplicate_assert_and_tautological_assert() {
    let source = include_str!("fixtures/redundant.py");
    let violations = violations_for(source);

    let rules: Vec<&str> = violations.iter().map(|v| v.rule).collect();
    assert!(
        rules.contains(&"NO_ASSERTIONS"),
        "expected NO_ASSERTIONS, got {rules:?}"
    );
    assert!(
        rules.contains(&"DUPLICATE_ASSERT"),
        "expected DUPLICATE_ASSERT, got {rules:?}"
    );
    assert!(
        rules.contains(&"TAUTOLOGICAL_ASSERT"),
        "expected TAUTOLOGICAL_ASSERT, got {rules:?}"
    );

    let no_assertions = violations
        .iter()
        .find(|v| v.rule == "NO_ASSERTIONS")
        .unwrap();
    assert_eq!(no_assertions.test_name, "test_stub_never_implemented");

    let smoke = violations
        .iter()
        .find(|v| v.rule == "NO_ASSERTIONS_SMOKE")
        .expect("a call-only test is a smoke warning");
    assert_eq!(smoke.test_name, "test_never_fails");
}

#[test]
fn does_not_flag_raises_context_distinct_asserts_real_comparisons_or_suppressed_lines() {
    let source = include_str!("fixtures/clean.py");
    let violations = violations_for(source);
    assert!(
        violations.is_empty(),
        "expected no violations, got {violations:?}"
    );
}
