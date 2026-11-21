use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordHint {
    pub source: &'static str,
    pub kind: &'static str,
    pub level: &'static str,
    pub summary: String,
    pub stamps: usize,
    pub markers: usize,
}

fn stamps() -> &'static [Regex] {
    static R: OnceLock<Vec<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        vec![
            Regex::new(r"\b\d{2}:\d{2}(?::\d{2})?Z\b").expect("stamp regex"),
            Regex::new(r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}").expect("iso regex"),
        ]
    })
}

const MARKERS: [&str; 4] = ["EXIT=", "exit=", "VERBATIM", "VISITS="];

pub fn record_hint(body: &str, key: &str) -> Option<RecordHint> {
    let mut ranges: Vec<(usize, usize)> = stamps()
        .iter()
        .flat_map(|re| re.find_iter(body).map(|m| (m.start(), m.end())))
        .collect();
    ranges.sort_unstable();
    let mut stamps = 0usize;
    let mut last_end = 0usize;
    for (a, b) in ranges {
        if a >= last_end {
            stamps += 1;
        }
        last_end = last_end.max(b);
    }
    let markers = MARKERS.iter().filter(|m| body.contains(*m)).count();
    if stamps < 3 && !(stamps >= 1 && markers >= 2) {
        return None;
    }
    let summary: String = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(500)
        .collect();
    Some(RecordHint {
        source: infer_source(body, key),
        kind: infer_kind(body),
        level: infer_level(body),
        summary,
        stamps,
        markers,
    })
}

fn infer_source(body: &str, key: &str) -> &'static str {
    let k = key.to_ascii_lowercase();
    for (needle, source) in [
        ("cluster-watch", "cluster-watch"),
        ("pr-watch", "pr-watch"),
        ("dispatch", "dispatch"),
        ("recess", "recess"),
        ("alerts", "alerts"),
        ("hygiene", "hygiene"),
        ("maintenance", "maintenance"),
        ("offsite", "offsite"),
    ] {
        if k.starts_with(needle) || k.contains(&format!("-{needle}")) {
            return source;
        }
    }
    let b = body.to_ascii_lowercase();
    for (needle, source) in [
        ("cluster-watch", "cluster-watch"),
        ("pr-watch", "pr-watch"),
        ("dispatch", "dispatch"),
        ("recess", "recess"),
        ("alerts", "alerts"),
        ("alert", "alerts"),
        ("hygiene", "hygiene"),
        ("maintenance", "maintenance"),
        ("offsite", "offsite"),
    ] {
        if b.contains(needle) {
            return source;
        }
    }
    "task"
}

fn infer_kind(body: &str) -> &'static str {
    let b = body.to_ascii_lowercase();
    static PASS: OnceLock<Regex> = OnceLock::new();
    let pass = PASS.get_or_init(|| Regex::new(r"(?i)\bpass\b").expect("pass regex"));
    if b.contains("firing") || b.contains("alertname") || b.contains("paged") {
        "alert"
    } else if pass.is_match(&b) {
        "pass"
    } else {
        "run"
    }
}

fn infer_level(body: &str) -> &'static str {
    let b = body.to_ascii_lowercase();
    static PRIORITY: OnceLock<Regex> = OnceLock::new();
    let priority = PRIORITY.get_or_init(|| Regex::new(r"(?i)\bp[01]\b").expect("priority regex"));
    if b.contains("critical") || priority.is_match(&b) {
        "critical"
    } else if b.contains("error") || b.contains("failed") {
        "error"
    } else if b.contains("warn") || b.contains("degraded") {
        "warn"
    } else {
        "info"
    }
}

pub fn refusal(hint: &RecordHint, key: &str) -> String {
    let quoted = hint.summary.replace('"', "'");
    format!(
        "this body is a RUN RECORD, not a memory: it carries {} timestamp(s) and {} run marker(s), \
which is the shape of a play-by-play (`{}`). A memory is a claim — true until superseded, found by \
meaning; an occurrence is found by time and source, and it belongs on the shelf the brain keeps for \
it. Write it there:\n\
\n    record append source={} kind={} level={} summary=\"{}\"\n\
\nand keep `detail` for the play-by-play. If it also carries a claim, write that claim here as one \
sentence. If this really is a memory, pass `force: true` and it will be written unchanged.\n\
\nIf the brain you are writing to has no `record` verb yet, pass `force: true` and say why in the \
body — a wall with no exit is how a rule gets routed around.",
        hint.stamps, hint.markers, key, hint.source, hint.kind, hint.level, quoted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_receipt_is_a_record() {
        let body = "EXIT=0 at 14:41Z\nchecked 5 repos\n14:42Z no change\n14:43Z no change";
        let h = record_hint(body, "pr-watch-run-2026-11-08").expect("a receipt");
        assert_eq!(h.source, "pr-watch");
        assert_eq!(h.kind, "run");
        assert_eq!(h.stamps, 3);
        assert!(h.summary.starts_with("EXIT=0"));
        let msg = refusal(&h, "pr-watch-run-2026-11-08");
        assert!(msg.contains("record append source=pr-watch"));
        assert!(msg.contains("force: true"));
        assert!(
            msg.contains("no `record` verb"),
            "the exit is in the message: {msg}"
        );
    }

    #[test]
    fn one_iso_stamp_counts_once() {
        let one = "MEASURED 2026-11-08 22:46Z.";
        assert!(
            record_hint(one, "a-lesson").is_none(),
            "one instant is one stamp"
        );
        let two = "MEASURED 2026-11-08 22:46Z and again 2026-11-09 01:04Z: both ok.";
        assert!(
            record_hint(two, "a-lesson").is_none(),
            "two cited measurements are a lesson, not a run"
        );
    }

    #[test]
    fn a_lesson_that_cites_one_clock_stamp_is_not_a_record() {
        let body =
            "★ MEASURED 2026-10-24 04:30Z: a tool that reports success must be checked at use.";
        assert!(record_hint(body, "hygiene-hygiene").is_none());
    }

    #[test]
    fn ordinary_prose_markers_do_not_make_a_record() {
        let body = "01:04Z the roll finished -> the turn settled later.";
        assert!(
            record_hint(body, "a-lesson").is_none(),
            "prose, not a receipt"
        );
    }

    #[test]
    fn one_stamp_and_two_receipt_markers_is_a_record() {
        let body = "14:41Z dispatch settled EXIT=0 with VERBATIM output";
        assert!(record_hint(body, "dispatch-2026-11-08").is_some());
    }

    #[test]
    fn a_claim_with_three_clock_stamps_is_refused_by_design() {
        let body =
            "14:41Z disk 93%\n14:50Z freed 20Gi\n15:02Z back to 61% — the cleanup cron was off";
        let h = record_hint(body, "alerts-2026-11-08-1441z").expect("refused by design");
        assert_eq!(h.stamps, 3);
    }

    #[test]
    fn a_password_is_not_a_pass() {
        let h = record_hint(
            "14:41Z rotated the password\n14:42Z EXIT=0\n14:43Z verified",
            "task-1",
        )
        .unwrap();
        assert_eq!(h.kind, "run", "`pass` is inside `password`");
    }

    #[test]
    fn the_summary_is_one_line_bounded_and_quote_safe() {
        let long = format!("14:41Z say \"hi\" {}\n14:42Z b\n14:43Z c", "x".repeat(800));
        let h = record_hint(&long, "task-1").unwrap();
        assert_eq!(h.summary.chars().count(), 500);
        assert!(!h.summary.contains('\n'));
        let msg = refusal(&h, "task-1");
        let line = msg
            .lines()
            .find(|l| l.contains("record append"))
            .expect("the paste line");
        let inner = line.split("summary=").nth(1).unwrap();
        assert_eq!(inner.matches('"').count(), 2, "balanced quotes: {line}");
    }
}
