use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

/// Counts from a pytest `--junitxml` report, split by whether the test body
/// actually executed. A `<failure>` still means the body ran (and so
/// contributed coverage); `<skipped>`/`<error>` mean it didn't.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct JunitSummary {
    pub ran: usize,
    pub skipped: usize,
    pub errored: usize,
}

pub fn load(path: &Path) -> anyhow::Result<JunitSummary> {
    let text = std::fs::read_to_string(path)?;
    parse(&text)
}

fn parse(xml: &str) -> anyhow::Result<JunitSummary> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut summary = JunitSummary::default();
    let mut in_testcase = false;
    let mut testcase_excluded = false;

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => match e.local_name().as_ref() {
                b"testcase" => {
                    in_testcase = true;
                    testcase_excluded = false;
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
                b"testcase" => summary.ran += 1,
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
                errored: 1
            }
        );
    }
}
