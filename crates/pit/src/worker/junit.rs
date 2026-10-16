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
    /// Places each kept failure points at, as the runner printed them, most
    /// likely first: the test case's `file`/`line`, then `path:line` in its
    /// failure text. `resolve` keeps the first that is a file in the repo.
    candidates: std::collections::HashMap<String, Vec<(String, u32)>>,
    /// Repo-relative `path:line` per failed test, after `resolve`.
    pub locations: std::collections::HashMap<String, (String, u32)>,
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
                let at = self
                    .locations
                    .get(name)
                    .map(|(p, l)| format!(" at {p}:{l}"))
                    .unwrap_or_default();
                if msg.is_empty() {
                    format!("{name}{at}")
                } else {
                    format!("{name} ({msg}){at}")
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

    /// Picks each failure's location among files in the repo (`files`,
    /// repo-relative). Runners print paths relative to the package, absolute
    /// in the workspace (`ws`) or in a container (`/workspace`).
    pub fn resolve(&mut self, files: &std::collections::HashSet<&str>, ws: &Path) {
        let ws = ws.to_string_lossy().replace('\\', "/");
        for (name, candidates) in &self.candidates {
            let found = candidates.iter().find_map(|(raw, line)| {
                let p = raw.replace('\\', "/");
                let p = p.strip_prefix("file://").unwrap_or(&p);
                let p = p
                    .strip_prefix(&format!("{ws}/"))
                    .or_else(|| p.strip_prefix("/workspace/"))
                    .unwrap_or(p);
                let p = p.trim_start_matches("./");
                if files.contains(p) {
                    return Some((p.to_string(), *line));
                }
                // Relative to a package directory: the one repo file that
                // ends with it.
                let suffix = format!("/{p}");
                let mut matches = files.iter().filter(|f| f.ends_with(&suffix));
                match (matches.next(), matches.next()) {
                    (Some(f), None) if !p.starts_with('/') => Some((f.to_string(), *line)),
                    _ => None,
                }
            });
            if let Some(loc) = found {
                self.locations.insert(name.clone(), loc);
            }
        }
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
        for (name, c) in other.candidates {
            self.candidates.entry(name).or_insert(c);
        }
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

/// Text scanned for locations, per failure.
const SCANNED: usize = 8 << 10;
/// Locations kept per failure.
const CANDIDATES: usize = 5;

/// `path:line` references in a failure's text: stack frames (`at f
/// (src/a.ts:42:7)`), assertion output (`tests/a_test.go:12:`) and Python's
/// `File "tests/test_a.py", line 12`. Only paths with an extension count.
pub fn locations_in(text: &str) -> Vec<(String, u32)> {
    let mut cut = text.len().min(SCANNED);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out: Vec<(String, u32)> = Vec::new();
    let mut push = |path: &str, line: &str| {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let Ok(line) = line.parse::<u32>() else {
            return;
        };
        if line > 0
            && name.contains('.')
            && out.len() < CANDIDATES
            && !out.iter().any(|(p, l)| p == path && *l == line)
        {
            out.push((path.to_string(), line));
        }
    };
    for l in text[..cut].lines() {
        if let Some((path, after)) = l
            .trim_start()
            .strip_prefix("File \"")
            .and_then(|r| r.split_once('"'))
        {
            if let Some(n) = after.trim_start_matches(',').trim().strip_prefix("line ") {
                let digits: String = n.chars().take_while(char::is_ascii_digit).collect();
                push(path, &digits);
            }
        }
        for token in l.split(|c: char| c.is_whitespace() || "()[]<>\"',".contains(c)) {
            let token = token.strip_prefix("file://").unwrap_or(token);
            // `src/a.ts:42:7`, `a_test.go:12:`, `C:\x\a.ts:3`: the path is
            // everything before the first part that is a line number.
            let parts: Vec<&str> = token.split(':').collect();
            if let Some(i) = (1..parts.len())
                .find(|&i| !parts[i].is_empty() && parts[i].bytes().all(|b| b.is_ascii_digit()))
            {
                let path = parts[..i].join(":");
                if !path.is_empty() {
                    push(&path, parts[i]);
                }
            }
        }
    }
    out
}

/// Counts `<testcase>` elements and their `<failure>`, `<error>` and
/// `<skipped>` children. Malformed input yields whatever was read so far.
pub fn parse(xml: &str) -> Summary {
    let mut reader = Reader::from_str(xml);
    let mut summary = Summary::default();
    let mut case: Option<String> = None;
    // The current test case's own `file` and `line`.
    let mut case_at: Option<(String, u32)> = None;
    // A failure whose message is in its text rather than an attribute.
    let mut pending: Option<(String, String)> = None;
    let mut text = String::new();
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
                    case_at = attr(&e, b"file").and_then(|f| {
                        let line: u32 = attr(&e, b"line")?.parse().ok()?;
                        // GitHub rejects a whole request over one line 0.
                        (line > 0).then_some((f, line))
                    });
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
                    text.clear();
                    // `<failure message="…"/>` has no end tag to record it at.
                    if !open {
                        record(&mut summary, pending.take(), case_at.clone(), "");
                    }
                }
            }
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"skipped" => {
                if case.is_some() {
                    summary.skipped += 1;
                }
            }
            // Failure text, plain or in CDATA (jest-junit writes CDATA).
            Event::Text(_) | Event::CData(_) if pending.is_some() => {
                let chunk = match &event {
                    Event::Text(t) => t.unescape().map(|c| c.to_string()).unwrap_or_default(),
                    Event::CData(c) => String::from_utf8_lossy(c).to_string(),
                    _ => String::new(),
                };
                if text.len() < SCANNED {
                    text.push_str(&chunk);
                }
                if let Some((_, msg)) = pending.as_mut().filter(|(_, m)| m.is_empty()) {
                    *msg = first_line(&chunk);
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"failure" | b"error" => {
                    record(&mut summary, pending.take(), case_at.clone(), &text)
                }
                b"testcase" => case = None,
                _ => {}
            },
            _ => {}
        }
    }
    summary
}

/// Adds a failure once per test, even with both `<failure>` and `<error>`.
fn record(
    summary: &mut Summary,
    failure: Option<(String, String)>,
    at: Option<(String, u32)>,
    text: &str,
) {
    if let Some(f) = failure {
        if summary.failed.insert(f.0.clone()) && summary.failures.len() < KEPT {
            let mut candidates: Vec<(String, u32)> = at.into_iter().collect();
            candidates.extend(locations_in(text));
            if !candidates.is_empty() {
                summary.candidates.insert(f.0.clone(), candidates);
            }
            summary.failures.push(f);
        }
    }
}

/// A report's text: a regular file, not followed through a symlink (a task
/// can swap one in for `/dev/zero`), and at most `MAX_REPORT_BYTES`.
fn read_report(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = open.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_REPORT_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_REPORT_BYTES).read_to_end(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// The combined summary of every report file, or `None` if there were none.
pub fn summarize(files: &[impl AsRef<Path>]) -> Option<Summary> {
    let mut total: Option<Summary> = None;
    for f in files {
        let Some(xml) = read_report(f.as_ref()) else {
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
    fn finds_where_a_failure_points() {
        assert_eq!(
            locations_in(
                "AssertionError: expected 90\n    at Object.<anonymous> (/workspace/src/cart.test.ts:42:7)\n    at node:internal/x:1:2"
            ),
            [("/workspace/src/cart.test.ts".to_string(), 42)]
        );
        assert_eq!(
            locations_in("Traceback:\n  File \"tests/test_cart.py\", line 12, in test_pay"),
            [("tests/test_cart.py".to_string(), 12)]
        );
        assert_eq!(
            locations_in("cart_test.go:31: got 1, want 2"),
            [("cart_test.go".to_string(), 31)]
        );
        assert_eq!(
            locations_in("at file:///home/u/w/a.ts:3:1 and C:\\w\\b.rs:9"),
            [
                ("/home/u/w/a.ts".to_string(), 3),
                ("C:\\w\\b.rs".to_string(), 9)
            ]
        );
        // Times and URLs are not places.
        assert!(locations_in("took 12:30 at http://localhost:8080/x").is_empty());
    }

    #[test]
    fn failures_resolve_to_repo_files() {
        // jest-junit: the stack in CDATA, paths absolute in the container.
        let xml = r#"<testsuites><testsuite name="cart">
<testcase classname="cart" name="pays"><failure><![CDATA[Error: card declined
    at pay (/workspace/packages/web/src/cart.ts:17:9)
    at Object.<anonymous> (/workspace/packages/web/src/cart.test.ts:42:7)]]></failure></testcase>
<testcase classname="t" name="py" file="tests/test_a.py" line="8"><failure message="assert 1 == 2"/></testcase>
<testcase classname="t" name="vendored"><failure>at x (node_modules/lib/index.js:3:1)</failure></testcase>
</testsuite></testsuites>"#;
        let mut s = parse(xml);
        assert_eq!(s.failures[0].1, "Error: card declined");
        let files: std::collections::HashSet<&str> = [
            "packages/web/src/cart.ts",
            "packages/web/src/cart.test.ts",
            "tests/test_a.py",
        ]
        .into_iter()
        .collect();
        s.resolve(&files, Path::new("/home/w/ws"));
        assert_eq!(
            s.locations["cart.pays"],
            ("packages/web/src/cart.ts".to_string(), 17)
        );
        assert_eq!(s.locations["t.py"], ("tests/test_a.py".to_string(), 8));
        assert!(!s.locations.contains_key("t.vendored"));
        assert!(
            s.message()
                .contains("cart.pays (Error: card declined) at packages/web/src/cart.ts:17"),
            "{}",
            s.message()
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

#[cfg(all(test, unix))]
mod report_files {
    use super::*;

    #[test]
    fn a_report_swapped_for_a_symlink_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.xml");
        std::fs::write(&real, r#"<testsuite><testcase name="a"/></testsuite>"#).unwrap();
        let link = dir.path().join("link.xml");
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        assert_eq!(summarize(&[&real]).map(|s| s.tests), Some(1));
        assert!(summarize(&[&link]).is_none());
    }
}
