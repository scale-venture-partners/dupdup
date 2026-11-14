use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

/// Counts from a pytest `--junitxml` report, split by whether the test body
/// actually executed. A `<failure>` still means the body ran (and so
/// contributed coverage); `<skipped>`/`<error>` mean it didn't.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct JunitSummary {
    /// Test cases that ran to completion, passed or failed.
    pub ran: usize,
    /// Test cases marked `<skipped>`.
    pub skipped: usize,
    /// Test cases marked `<error>` (failed before the body ran, e.g. a
    /// fixture error).
    pub errored: usize,
    /// The `classname::name` of each test counted in `ran`, the form
    /// [`junit_key`] produces from a pytest node id.
    pub ran_tests: Vec<String>,
}

/// The junit `classname::name` of a pytest node id:
/// `tests/test_a.py::TestB::test_c[p]` becomes `tests.test_a.TestB::test_c[p]`.
/// The parametrize suffix is split off before the `::` split because a
/// parametrize id can itself contain `::` or `/`.
#[must_use]
pub fn junit_key(node_id: &str) -> String {
    let (head, params) = match node_id.find('[') {
        Some(i) => node_id.split_at(i),
        None => (node_id, ""),
    };
    let mut parts: Vec<&str> = head.split("::").collect();
    let name = parts.pop().unwrap_or("");
    let module = parts.first().copied().unwrap_or("");
    let module = module
        .strip_suffix(".py")
        .unwrap_or(module)
        .replace('/', ".");
    let classes = &parts[parts.len().min(1)..];
    let classname = std::iter::once(module.as_str())
        .chain(classes.iter().copied())
        .collect::<Vec<_>>()
        .join(".");
    format!("{classname}::{name}{params}")
}

/// Tests that ran but whose own body covered nothing in the measured code:
/// the junit tests in `summary` whose key is not in `analyzed_ids`.
#[must_use]
pub fn tests_without_coverage<'a>(
    summary: &JunitSummary,
    analyzed_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let covered: std::collections::HashSet<String> =
        analyzed_ids.into_iter().map(junit_key).collect();
    let mut missing: Vec<String> = summary
        .ran_tests
        .iter()
        .filter(|k| !covered.contains(*k))
        .cloned()
        .collect();
    missing.sort();
    missing
}

/// Loads and summarizes a pytest `--junitxml` report at `path`.
///
/// # Errors
/// Returns an error if `path` can't be read or isn't valid XML.
pub fn load(path: &Path) -> anyhow::Result<JunitSummary> {
    let text = std::fs::read_to_string(path)?;
    parse(&text)
}

fn case_key(e: &quick_xml::events::BytesStart) -> String {
    let attr = |name: &[u8]| {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
            .unwrap_or_default()
    };
    format!("{}::{}", attr(b"classname"), attr(b"name"))
}

fn parse(xml: &str) -> anyhow::Result<JunitSummary> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut summary = JunitSummary::default();
    let mut in_testcase = false;
    let mut testcase_excluded = false;
    let mut current_key = String::new();

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => match e.local_name().as_ref() {
                b"testcase" => {
                    in_testcase = true;
                    testcase_excluded = false;
                    current_key = case_key(&e);
                }
                b"skipped" if in_testcase => {
                    testcase_excluded = true;
                    summary.skipped += 1;
                }
                b"error" if in_testcase => {
                    testcase_excluded = true;
                    summary.errored += 1;
                }
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                b"testcase" => {
                    summary.ran += 1;
                    summary.ran_tests.push(case_key(&e));
                }
                b"skipped" if in_testcase => {
                    testcase_excluded = true;
                    summary.skipped += 1;
                }
                b"error" if in_testcase => {
                    testcase_excluded = true;
                    summary.errored += 1;
                }
                _ => {}
            },
            Event::End(e) => {
                if e.local_name().as_ref() == b"testcase" {
                    if !testcase_excluded {
                        summary.ran += 1;
                        summary.ran_tests.push(std::mem::take(&mut current_key));
                    }
                    in_testcase = false;
                }
            }
            _ => {}
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_passed_failed_skipped_and_errored() {
        let xml = r#"<?xml version="1.0"?>
        <testsuites>
        <testsuite tests="4">
            <testcase classname="t" name="passed" time="0.01"/>
            <testcase classname="t" name="failed" time="0.01">
                <failure message="boom">traceback</failure>
            </testcase>
            <testcase classname="t" name="skipped" time="0.0">
                <skipped message="not run"/>
            </testcase>
            <testcase classname="t" name="errored" time="0.0">
                <error message="setup blew up">traceback</error>
            </testcase>
        </testsuite>
        </testsuites>"#;
        let summary = parse(xml).unwrap();
        assert_eq!(
            summary,
            JunitSummary {
                ran: 2,
                skipped: 1,
                errored: 1,
                ran_tests: vec!["t::passed".into(), "t::failed".into()],
            }
        );
    }

    #[test]
    fn junit_key_maps_a_node_id_to_classname_and_name() {
        assert_eq!(junit_key("tests/test_a.py::test_b"), "tests.test_a::test_b");
        assert_eq!(
            junit_key("tests/test_a.py::TestC::test_d[x/y::z]"),
            "tests.test_a.TestC::test_d[x/y::z]"
        );
    }

    #[test]
    fn tests_without_coverage_is_the_ran_tests_missing_from_the_analysis() {
        let summary = JunitSummary {
            ran_tests: vec!["tests.test_a::test_x".into(), "tests.test_a::test_y".into()],
            ..JunitSummary::default()
        };
        let missing = tests_without_coverage(&summary, ["tests/test_a.py::test_x"]);
        assert_eq!(missing, vec!["tests.test_a::test_y"]);
    }
}
