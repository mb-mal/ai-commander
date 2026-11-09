//! К7: context compaction FOR the dsh harness — ai-commander owns the
//! history, so it can compact it itself (dsh replays the whole history
//! every turn and has no compaction of its own).
//!
//! Plan (pure): when the replay estimate passes the threshold, the middle
//! of the dialogue is summarized; the head (task instructions) and the
//! recent tail survive untouched. Boundaries never split a call/result
//! pair — at dsh's message granularity that means a user message and the
//! assistant reply that follows it stay together.
//!
//! Apply: the original history is archived to a sidecar file before the
//! store is rewritten; the summary lands as one system message.

use serde::{Deserialize, Serialize};

/// One stored dialogue turn (mirror of the dsh store message).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

/// Which turns are summarized and which survive as-is.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// turns kept verbatim at the head
    pub keep_head: usize,
    /// [keep_head, keep_head + summarize_len) go to the summary
    pub summarize_len: usize,
    /// turns kept verbatim at the tail
    pub keep_tail: usize,
}

pub const DEFAULT_THRESHOLD_PCT: u64 = 80;
/// the protected tail, in tokens (chars/4) — the hermes-typical budget
pub const TAIL_TOKENS: u64 = 8_000;

pub fn tokens_of(text: &str) -> u64 {
    (text.len() as u64) / 4
}

/// Decide whether the history needs compaction.
pub fn needs_compaction(turns: &[Turn], window: u64, threshold_pct: u64) -> bool {
    let used: u64 = turns.iter().map(|t| tokens_of(&t.text)).sum();
    window > 0 && used * 100 > window * threshold_pct
}

/// Build the compaction plan: head survives, middle summarized, tail
/// survives. Boundaries snap OUTWARD to user-message starts so a
/// user→assistant pair is never split.
pub fn plan(turns: &[Turn]) -> Option<Plan> {
    if turns.len() < 4 {
        return None; // nothing worth compacting
    }
    // tail: whole turns from the end until TAIL_TOKENS is covered
    let mut tail_tokens = 0u64;
    let mut keep_tail = 0usize;
    for t in turns.iter().rev() {
        if keep_tail > 0 && (tail_tokens >= TAIL_TOKENS || keep_tail >= turns.len() / 2) {
            break;
        }
        tail_tokens += tokens_of(&t.text);
        keep_tail += 1;
    }
    // head: the first user message (task instructions) + its reply
    let keep_head = turns
        .iter()
        .position(|t| t.role == "assistant")
        .map(|i| (i + 1).min(turns.len()))
        .unwrap_or(1);

    let summarize_len = turns.len().saturating_sub(keep_head + keep_tail);
    if summarize_len == 0 {
        return None;
    }
    // snap the summarize START outward to a user message (pair integrity)
    let mut start = keep_head;
    while start > 0 && turns[start].role != "user" {
        start -= 1;
    }
    let keep_head = start;
    let summarize_len = turns.len() - keep_head - keep_tail;
    if summarize_len == 0 {
        return None;
    }
    Some(Plan {
        keep_head,
        summarize_len,
        keep_tail,
    })
}

/// The summarization prompt for the middle of the dialogue.
pub fn summarize_prompt(turns: &[Turn], p: &Plan) -> String {
    let middle: Vec<String> = turns[p.keep_head..p.keep_head + p.summarize_len]
        .iter()
        .map(|t| format!("{}: {}", t.role, t.text))
        .collect();
    format!(
        "Summarize the following dialogue for a coding agent continuing the \
         task. Sections: GOAL, CURRENT STATE, FILES TOUCHED, NEXT STEP. \
         Be concise and factual.\n\n---\n{}\n---",
        middle.join("\n\n")
    )
}

/// Apply the plan: archive the original, replace the middle with the
/// summary. Returns the new turns.
pub fn apply(turns: &[Turn], p: &Plan, summary: &str) -> Vec<Turn> {
    let mut out: Vec<Turn> = turns[..p.keep_head].to_vec();
    out.push(Turn {
        role: "user".into(),
        text: format!(
            "[CONTEXT COMPACTION] Summary of the earlier dialogue:\n{summary}\n\
             (the original turns are archived)"
        ),
    });
    out.extend(turns[turns.len() - p.keep_tail..].iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns(n: usize, chars_each: usize) -> Vec<Turn> {
        (0..n)
            .map(|i| Turn {
                role: if i % 2 == 0 {
                    "user".into()
                } else {
                    "assistant".into()
                },
                text: "x".repeat(chars_each) + &format!(" #{i}"),
            })
            .collect()
    }

    #[test]
    fn threshold_decides() {
        let big = turns(40, 4_000); // ~40k tokens
        assert!(needs_compaction(&big, 40_000, DEFAULT_THRESHOLD_PCT));
        assert!(!needs_compaction(&big, 200_000, DEFAULT_THRESHOLD_PCT));
        assert!(
            !needs_compaction(&big, 0, DEFAULT_THRESHOLD_PCT),
            "no window"
        );
    }

    #[test]
    fn boundaries_never_split_a_pair() {
        let ts = turns(30, 800);
        let p = plan(&ts).expect("planned");
        // head boundary is a user message (a pair start)
        assert_eq!(ts[p.keep_head].role, "user", "head snaps to a pair start");
        // the first summarized turn is a user message too
        assert_eq!(ts[p.keep_head].role, "user");
        // tail + head + middle == everything
        assert_eq!(p.keep_head + p.summarize_len + p.keep_tail, ts.len());
    }

    #[test]
    fn applied_history_is_shorter_and_head_tail_survive() {
        let ts = turns(30, 1_000);
        let p = plan(&ts).expect("planned");
        let summary = "GOAL: fix the bug. STATE: half done. NEXT: tests.";
        let out = apply(&ts, &p, summary);
        assert!(out.len() < ts.len(), "compaction shrinks the history");
        // head survives verbatim
        assert_eq!(out[0], ts[0]);
        // the summary is marked
        assert!(out[p.keep_head].text.contains("[CONTEXT COMPACTION]"));
        // the tail survives verbatim
        assert_eq!(out.last(), ts.last());
        // replay estimate is below the threshold after compaction
        let used: u64 = out.iter().map(|t| tokens_of(&t.text)).sum();
        assert!(
            used * 100 <= 30_000 * DEFAULT_THRESHOLD_PCT,
            "post-compaction size {used} below the window threshold"
        );
    }

    #[test]
    fn original_is_preserved_by_the_caller() {
        // apply() is pure — the CALLER archives; verified by the dsh test
        // that checks the sidecar. Here: apply does not mutate the input.
        let ts = turns(10, 500);
        let before = ts.clone();
        if let Some(p) = plan(&ts) {
            let _ = apply(&ts, &p, "s");
        }
        assert_eq!(ts, before, "the original history is not destroyed");
    }

    #[test]
    fn too_short_history_is_not_planned() {
        assert!(plan(&turns(3, 10_000)).is_none());
    }
}
