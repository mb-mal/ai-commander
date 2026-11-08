//! OpenTelemetry export (review "Позже"): write board events as OTLP-compatible
//! JSON spans to a file, ready for `otelcol --config file_exporter` ingestion.
//! Zero-dependency: spans are plain OTLP/JSON bodies.

use anyhow::Result;
use serde_json::json;

use crate::kanban::store::StoredEvent;

/// Append events as OTLP JSON export lines to `<dir>/ai-commander-spans.jsonl`.
pub fn export_events(
    dir: &std::path::Path,
    events: &[StoredEvent],
    service: &str,
) -> Result<usize> {
    use std::io::Write;
    std::fs::create_dir_all(dir).ok();
    let path = dir.join("ai-commander-spans.jsonl");
    let mut out = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?,
    );
    for ev in events {
        let span = json!({
            "traceId": trace_id(&ev.task_id),
            "spanId": span_id(ev.seq),
            "name": format!("kanban.{}", ev.action.split(':').next().unwrap_or("event")),
            "kind": "SPAN_KIND_INTERNAL",
            "startTimeUnixNano": ev.ts_ms.wrapping_mul(1_000_000),
            "endTimeUnixNano": ev.ts_ms.wrapping_mul(1_000_000),
            "attributes": [
                {"key": "task.id", "value": {"stringValue": ev.task_id}},
                {"key": "board.action", "value": {"stringValue": ev.action.clone()}},
                {"key": "service.name", "value": {"stringValue": service}},
            ],
            "status": {"code": "STATUS_CODE_OK"},
        });
        let resource_spans = json!({
            "resourceSpans": [{
                "resource": {"attributes": [
                    {"key": "service.name", "value": {"stringValue": service}}
                ]},
                "scopeSpans": [{"spans": [span]}]
            }]
        });
        writeln!(out, "{resource_spans}")?;
    }
    out.flush().ok();
    Ok(events.len())
}

fn trace_id(task: &str) -> String {
    // deterministic 32-hex trace per task
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in task.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:032x}")
}

fn span_id(seq: u64) -> String {
    format!("{:016x}", seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::{Comment, Task, TaskStatus};
    use serde_json::Value;

    fn ev(seq: u64) -> StoredEvent {
        StoredEvent {
            seq,
            ts_ms: 1_700_000_000_000,
            task_id: "t-0001".into(),
            action: "transition:Start".into(),
            task: Some(Task {
                id: "t-0001".into(),
                title: "x".into(),
                description: String::new(),
                acceptance: Vec::new(),
                status: TaskStatus::InProgress,
                harness: Some("oc".into()),
                role: None,
                comments: vec![Comment {
                    author: "a".into(),
                    kind: "comment".into(),
                    text: "t".into(),
                    ts_ms: 1,
                }],
                depends_on: Vec::new(),
                session_id: None,
                attempts: 1,
                started_ms: None,
                lease_until_ms: None,
                max_attempts: 3,
                schedule: String::new(),
                budget_usd: 0.0,
                unit_cost_usd: 0.0,
                run_budget_secs: 0,
                parent_id: None,
                max_turns: 0,
                created_ms: 1,
                updated_ms: 1,
            }),
        }
    }

    #[test]
    fn writes_otlp_lines() {
        let dir = std::env::temp_dir().join(format!("aic-otel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let n = export_events(&dir, &[ev(1), ev(2)], "ai-commander").unwrap();
        assert_eq!(n, 2);
        let raw = std::fs::read_to_string(dir.join("ai-commander-spans.jsonl")).unwrap();
        let first: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(
            first["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"],
            "kanban.transition"
        );
        assert_eq!(
            first["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "ai-commander"
        );
        // trace ids differ per task, span ids differ per seq
        let second: Value = serde_json::from_str(raw.lines().nth(1).unwrap()).unwrap();
        assert_ne!(
            first["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["spanId"],
            second["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["spanId"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
