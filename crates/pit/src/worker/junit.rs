//! Reads JUnit XML test reports (`reports` on a task), so a failure names the
//! tests that failed instead of leaving people to scroll the log. Nearly every
//! test runner can write this format: Jest, Vitest, pytest, Go (gotestsum),
//! JUnit, cargo-nextest, PHPUnit.

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

const SHOWN: usize = 5;
const MESSAGE_CHARS: usize = 120;

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub tests: u64,
    pub skipped: u64,
    /// `suite.test` and the first line of its failure message.
    pub failures: Vec<(String, String)>,
}

impl Summary {
    pub fn failed(&self) -> u64 {
        self.failures.len() as u64
    }

    /// "2 of 120 tests failed: a.b (expected 1); c.d (timeout)".
    pub fn message(&self) -> String {
        if self.failures.is_empty() {
            return format!("{} tests passed", self.tests - self.skipped);
        }
        let shown: Vec<String> = self
            .failures
            .iter()
            .take(SHOWN)
            .map(|(name, msg)| {
                if msg.is_empty() {
                    name.clone()
                } else {
                    format!("{name} ({msg})")
                }
            })
            .collect();
        let more = self.failures.len().saturating_sub(SHOWN);
        let tail = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        format!(
            "{} of {} tests failed: {}{tail}",
            self.failed(),
            self.tests,
            shown.join("; ")
        )
    }

    fn merge(&mut self, other: Summary) {
        self.tests += other.tests;
        self.skipped += other.skipped;
        self.failures.extend(other.failures);
    }
}

fn attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| a.unescape_value().ok())
        .map(|v| v.to_string())
}

fn first_line(s: &str) -> String {
    let line = s.trim().lines().next().unwrap_or_default().trim();
    if line.chars().count() > MESSAGE_CHARS {
        let cut: String = line.chars().take(MESSAGE_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// Counts `<testcase>` elements and their `<failure>`, `<error>` and
/// `<skipped>` children. Malformed input yields whatever was read so far.
pub fn parse(xml: &str) -> Summary {
    let mut reader = Reader::from_str(xml);
    let mut summary = Summary::default();
    let mut case: Option<String> = None;
    // A failure whose message is in its text rather than an attribute.
    let mut pending: Option<(String, String)> = None;
    loop {
        let event = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(e) => e,
        };
        let open = matches!(event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"testcase" => {
                summary.tests += 1;
                let name = attr(&e, b"name").unwrap_or_default();
                let full = match attr(&e, b"classname").filter(|c| !c.is_empty()) {
                    Some(class) => format!("{class}.{name}"),
                    None => name,
                };
                // `<testcase/>` has no children (it passed); only an open tag
                // can hold a failure.
                if open {
                    case = Some(full);
                }
            }
            Event::Start(e) | Event::Empty(e)
                if matches!(e.name().as_ref(), b"failure" | b"error") =>
            {
                if let Some(name) = &case {
                    let msg = attr(&e, b"message")
                        .map(|m| first_line(&m))
                        .unwrap_or_default();
                    pending = Some((name.clone(), msg));
                    // `<failure message="…"/>` has no end tag to record it at.
                    if !open {
                        record(&mut summary, pending.take());
                    }
                }
            }
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"skipped" => {
                if case.is_some() {
                    summary.skipped += 1;
                }
            }
            Event::Text(t) => {
                if let Some((_, msg)) = pending.as_mut().filter(|(_, m)| m.is_empty()) {
                    if let Ok(text) = t.unescape() {
                        *msg = first_line(&text);
                    }
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"failure" | b"error" => record(&mut summary, pending.take()),
                b"testcase" => case = None,
                _ => {}
            },
            _ => {}
        }
    }
    summary
}

/// Adds a failure once per test, even with both `<failure>` and `<error>`.
fn record(summary: &mut Summary, failure: Option<(String, String)>) {
    if let Some(f) = failure {
        if !summary.failures.iter().any(|(n, _)| *n == f.0) {
            summary.failures.push(f);
        }
    }
}

/// The combined summary of every report file, or `None` if there were none.
pub fn summarize(files: &[impl AsRef<Path>]) -> Option<Summary> {
    let mut total: Option<Summary> = None;
    for f in files {
        let Ok(xml) = std::fs::read_to_string(f) else {
            continue;
        };
        total
            .get_or_insert_with(Default::default)
            .merge(parse(&xml));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_failed_tests() {
        let xml = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="cart" tests="4">
    <testcase classname="cart" name="adds items"/>
    <testcase classname="cart" name="applies coupons">
      <failure message="expected 90 to equal 80&#10;at cart.test.ts:42">AssertionError</failure>
    </testcase>
    <testcase classname="cart" name="checks out"><error>TypeError: x is undefined
    at checkout (cart.ts:9)</error></testcase>
    <testcase classname="cart" name="later"><skipped/></testcase>
  </testsuite>
</testsuites>"#;
        let s = parse(xml);
        assert_eq!(s.tests, 4);
        assert_eq!(s.skipped, 1);
        assert_eq!(
            s.failures,
            [
                (
                    "cart.applies coupons".to_string(),
                    "expected 90 to equal 80".to_string()
                ),
                (
                    "cart.checks out".to_string(),
                    "TypeError: x is undefined".to_string()
                ),
            ]
        );
        assert_eq!(
            s.message(),
            "2 of 4 tests failed: cart.applies coupons (expected 90 to equal 80); cart.checks out (TypeError: x is undefined)"
        );
    }

    #[test]
    fn a_clean_report_and_garbage() {
        let s =
            parse(r#"<testsuite><testcase name="a"/><testcase name="b"></testcase></testsuite>"#);
        assert_eq!((s.tests, s.failed()), (2, 0));
        assert_eq!(s.message(), "2 tests passed");
        assert_eq!(parse("not xml at all <<<").tests, 0);
        let s = parse(
            r#"<testsuite><testcase classname="c" name="t"><failure message="no"/></testcase></testsuite>"#,
        );
        assert_eq!(s.failures, [("c.t".to_string(), "no".to_string())]);
    }
}
