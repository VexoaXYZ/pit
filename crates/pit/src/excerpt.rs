//! Short, error-focused excerpts of task logs, for the CLI and coding agents.
//!
//! The tail of a log is often the wrong place to look: a type checker prints
//! its error first and then thousands of lines of progress. An agent handed the
//! last N lines gets noise and misses the error, while paying for every token.
//! So this keeps lines that look like errors (with a little context) plus the
//! last few lines, shortens very long lines, and marks what was left out.

const CONTEXT: usize = 2;
const TAIL: usize = 8;
const MAX_LINE: usize = 240;

fn looks_like_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    // Summaries that report zero problems are not errors. "0 errors" must
    // not match inside "10 errors".
    let zero = [
        "0 errors",
        "0 error",
        "0 failed",
        "0 failures",
        "0 problems",
    ]
    .iter()
    .any(|z| {
        l.match_indices(z)
            .any(|(i, _)| i == 0 || !l.as_bytes()[i - 1].is_ascii_digit())
    });
    if zero || l.contains("no errors") {
        return false;
    }
    [
        "error",
        "fail",
        "panic",
        "exception",
        "traceback",
        "assert",
        "fatal",
        "✗",
        "×",
        "✘",
    ]
    .iter()
    .any(|k| l.contains(k))
}

fn shorten(line: &str) -> String {
    if line.chars().count() <= MAX_LINE {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE).collect();
    format!("{cut}… [{} more chars]", line.chars().count() - MAX_LINE)
}

/// At most `max_lines` lines of `log`, preferring error lines and the tail.
pub fn excerpt(log: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let n = lines.len();
    let mut keep = vec![false; n];
    let mut budget = max_lines;
    // The tail first: it usually holds the summary.
    for k in keep.iter_mut().skip(n.saturating_sub(TAIL)) {
        *k = true;
    }
    budget = budget.saturating_sub(TAIL.min(n));
    // Then error lines with context, earliest first, while the budget lasts.
    for i in (0..n).filter(|&i| looks_like_error(lines[i])) {
        let (from, to) = (i.saturating_sub(CONTEXT), (i + CONTEXT + 1).min(n));
        let new = (from..to).filter(|&j| !keep[j]).count();
        if new > budget {
            break;
        }
        for k in keep.iter_mut().take(to).skip(from) {
            *k = true;
        }
        budget -= new;
    }
    let mut out = Vec::new();
    let mut skipped = 0;
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if skipped > 0 {
                out.push(format!("… {skipped} lines omitted …"));
                skipped = 0;
            }
            out.push(shorten(line));
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push(format!("… {skipped} lines omitted …"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_an_early_error_that_a_tail_would_miss() {
        let mut log = String::new();
        for i in 0..400 {
            log.push_str(&format!("[build] module {i}\n"));
        }
        log.push_str(
            "src/a.ts(42,7): error TS2322: Type 'string' is not assignable to type 'number'.\n",
        );
        for i in 400..20000 {
            log.push_str(&format!("[build] module {i} {}\n", "0".repeat(300)));
        }
        log.push_str("Found 1 error in 1 file.\n");
        let ex = excerpt(&log, 40);
        assert!(ex.contains("TS2322"), "{ex}");
        assert!(ex.contains("Found 1 error"));
        assert!(ex.contains("lines omitted"));
        assert!(ex.lines().count() <= 40 + 3, "{}", ex.lines().count());
        assert!(ex.len() < 4_000, "excerpt is {} chars", ex.len());
    }

    #[test]
    fn zero_error_summaries_are_not_errors() {
        assert!(!looks_like_error(
            "Found 0 errors. Watching for file changes."
        ));
        assert!(looks_like_error("FAIL src/a.test.ts > adds"));
        assert!(looks_like_error("Found 10 errors in 3 files."));
    }
}
