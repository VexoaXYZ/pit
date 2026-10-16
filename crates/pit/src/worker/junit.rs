//! Reads JUnit XML test reports (`reports` on a task), so a failure names the
//! tests that failed instead of leaving people to scroll the log. Nearly every
//! test runner can write this format: Jest, Vitest, pytest, Go (gotestsum),
//! JUnit, cargo-nextest, PHPUnit.

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

const SHOWN: usize = 5;
const MESSAGE_CHARS: usize = 120;
/// Failures kept with their message; the rest are only counted.
const KEPT: usize = 100;
/// Report files larger than this are skipped: reading one is a fork's
/// cheapest way to hold a worker after its task's timeout has passed.
pub const MAX_REPORT_BYTES: u64 = 64 << 20;

#[derive(Debug, Default)]
pub struct Summary {
    pub tests: u64,
    pub skipped: u64,
    /// `suite.test` and the first line of its failure message, for the
    /// first `KEPT` failed tests.
    pub failures: Vec<(String, String)>,
    /// Every failed test, once.
    failed: std::collections::HashSet<String>,
    /// Seconds per `<testsuite name>` (Vitest, Playwright and most others
    /// name suites after the test file) or `<testcase file>`.
    pub file_times: std::collections::HashMap<String, f64>,
}

impl Summary {
    pub fn failed(&self) -> u64 {
        self.failed.len() as u64
    }

    /// "2 of 120 tests failed: a.b (expected 1); c.d (timeout)".
    pub fn message(&self) -> String {
        if self.failures.is_empty() {
            return format!("{} tests passed", self.tests.saturating_sub(self.skipped));
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
        let more = (self.failed() as usize).saturating_sub(SHOWN.min(self.failures.len()));
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
        for f in other.failures {
            if self.failures.len() < KEPT && !self.failed.contains(&f.0) {
                self.failures.push(f);
            }
        }
        self.failed.extend(other.failed);
        for (f, t) in other.file_times {
            *self.file_times.entry(f).or_default() += t;
        }
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
        if let Event::Start(e) | Event::Empty(e) = &event {
            let time = || attr(e, b"time").and_then(|t| t.parse::<f64>().ok());
            match e.name().as_ref() {
                b"testsuite" => {
                    if let (Some(name), Some(t)) = (attr(e, b"name"), time()) {
                        *summary.file_times.entry(name).or_default() += t;
                    }
                }
                b"testcase" => {
                    if let (Some(file), Some(t)) = (attr(e, b"file"), time()) {
                        *summary.file_times.entry(file).or_default() += t;
                    }
                }
                _ => {}
            }
        }
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
        if summary.failed.insert(f.0.clone()) && summary.failures.len() < KEPT {
            summary.failures.push(f);
        }
    }
}

/// The combined summary of every report file, or `None` if there were none.
pub fn summarize(files: &[impl AsRef<Path>]) -> Option<Summary> {
    let mut total: Option<Summary> = None;
    for f in files {
        if std::fs::metadata(f).map_or(true, |m| m.len() > MAX_REPORT_BYTES) {
            continue;
        }
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
    fn huge_and_odd_reports_stay_cheap() {
        // Many failures: counted in linear time, a bounded number kept.
        let mut xml = String::from("<testsuite>");
        for i in 0..200_000 {
            xml.push_str(&format!(
                r#"<testcase name="t{i}"><failure message="x"/></testcase>"#
            ));
        }
        xml.push_str("</testsuite>");
        let t = std::time::Instant::now();
        let s = parse(&xml);
        assert!(t.elapsed().as_secs() < 10, "{:?}", t.elapsed());
        assert_eq!(s.failed(), 200_000);
        assert_eq!(s.failures.len(), KEPT);
        assert!(s.message().ends_with("and 199995 more"), "{}", s.message());
        // Two <skipped/> in one case cannot underflow the passed count.
        let s =
            parse(r#"<testsuite><testcase name="a"><skipped/><skipped/></testcase></testsuite>"#);
        assert_eq!(s.message(), "0 tests passed");
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
