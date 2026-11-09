//! К8: the pure context-map model for the DEFRAG compaction view.
//!
//! Turns (session history, `used`/`size` tokens, map dimensions) into a
//! flat `Vec<Block>` — one block per map cell, left-to-right top-to-bottom
//! from the window start to its end. No terminal, no time, no I/O: the
//! animator and the tests consume this deterministically.

use crate::backend::ChatMsg;

/// One map cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// system prompt / instructions / tool schemas — compaction never moves it
    Pinned,
    /// an ordinary old turn of the dialogue
    Turn,
    /// a tool output that was pruned without summarization
    Pruned,
    /// the compacted summary
    Summary,
    /// the protected recent tail
    Recent,
    /// free window
    Free,
}

/// A segment of the session history used to size the map regions.
#[derive(Debug, Clone, Default)]
pub struct HistoryShape {
    /// approximate tokens of the pinned head (system prompt etc.)
    pub pinned_tokens: u64,
    /// one entry per remaining message: its approximate token size and
    /// whether it is a tool output (prunable)
    pub messages: Vec<(u64, bool)>,
    /// approximate tokens of the protected recent tail (the last messages
    /// that survive compaction untouched)
    pub recent_tokens: u64,
}

impl HistoryShape {
    /// Estimate from the rendered chat messages: chars / 4 per message,
    /// tool-kind parts count as tool outputs.
    pub fn estimate(msgs: &[ChatMsg], pinned_chars: usize) -> HistoryShape {
        let pinned_tokens = (pinned_chars as u64) / 4;
        let messages: Vec<(u64, bool)> = msgs
            .iter()
            .map(|m| {
                let chars: usize = m.parts.iter().map(|p| p.text.len()).sum::<usize>() + 8;
                let is_tool = m
                    .parts
                    .iter()
                    .all(|p| p.kind == crate::backend::PartKind::Tool)
                    && !m.parts.is_empty();
                ((chars as u64) / 4, is_tool)
            })
            .collect();
        // a typical harness tail budget; bounded by the history itself
        let total: u64 = messages.iter().map(|(t, _)| *t).sum();
        let recent_tokens = total.min(8_000);
        HistoryShape {
            pinned_tokens,
            messages,
            recent_tokens,
        }
    }
}

/// Tokens per one map block for the given window and map size.
pub fn tokens_per_block(size: u64, map_cells: usize) -> u64 {
    if map_cells <= 1 {
        return size.max(1);
    }
    size.div_ceil(map_cells as u64)
}

/// Build the map: the state of the context window BEFORE compaction.
///
/// Regions: pinned head, ordinary turns, the protected tail; tool outputs
/// inside the old region render as `Pruned` candidates only when
/// `pruned` is true (the "Unfragment Files Only" pass).
pub fn build_before(
    used: u64,
    size: u64,
    shape: &HistoryShape,
    map_cells: usize,
    pruned: bool,
) -> Vec<Block> {
    // К8: the shape gives PROPORTIONS, `used` the total — segments are
    // scaled so the occupied cells equal ceil(used/per) within one block.
    let per = tokens_per_block(size, map_cells).max(1);
    let mut blocks: Vec<Block> = Vec::with_capacity(map_cells);

    // where does the protected tail start? whole messages, from the end,
    // until their sum covers `recent_tokens`
    let mut acc: u64 = 0;
    let mut tail_start = shape.messages.len();
    for (i, (t, _)) in shape.messages.iter().enumerate().rev() {
        acc = acc.saturating_add(*t);
        tail_start = i;
        if acc >= shape.recent_tokens {
            break;
        }
    }
    let tail_actual: u64 = shape.messages[tail_start.min(shape.messages.len())..]
        .iter()
        .map(|(t, _)| *t)
        .sum();
    let old_msgs = &shape.messages[..tail_start.min(shape.messages.len())];
    let pinned_raw = shape.pinned_tokens.min(used);

    let total_shape: u64 = pinned_raw
        + old_msgs
            .iter()
            .map(|(t, _)| *t)
            .sum::<u64>()
            .saturating_add(tail_actual);
    let scale = if total_shape == 0 {
        1.0
    } else {
        used as f64 / total_shape as f64
    };

    let mut cursor_tokens: u64 = 0;
    let mut place = |tokens: u64, cell: Block, blocks: &mut Vec<Block>| {
        let scaled = (tokens as f64 * scale) as u64;
        cursor_tokens = cursor_tokens.saturating_add(scaled);
        let target = cursor_tokens.div_ceil(per) as usize;
        while blocks.len() < target.min(map_cells) {
            blocks.push(cell);
        }
    };
    place(pinned_raw, Block::Pinned, &mut blocks);
    for (t, is_tool) in old_msgs {
        place(
            *t,
            if pruned && *is_tool {
                Block::Pruned
            } else {
                Block::Turn
            },
            &mut blocks,
        );
    }
    place(tail_actual, Block::Recent, &mut blocks);

    while blocks.len() < map_cells {
        blocks.push(Block::Free);
    }
    blocks.truncate(map_cells);
    blocks
}

/// Build the map AFTER compaction from real numbers: pinned → summary →
/// recent → free (the DOS "optimized" layout).
pub fn build_after(
    used_after: u64,
    size: u64,
    shape: &HistoryShape,
    summary_tokens: u64,
    map_cells: usize,
) -> Vec<Block> {
    let per = tokens_per_block(size, map_cells).max(1);
    let mut blocks: Vec<Block> = Vec::with_capacity(map_cells);
    let pinned = shape.pinned_tokens.min(used_after);
    let recent = shape
        .recent_tokens
        .min(used_after.saturating_sub(pinned + summary_tokens));

    let mut cursor_tokens: u64 = 0;
    let mut place = |tokens: u64, cell: Block, blocks: &mut Vec<Block>| {
        cursor_tokens = cursor_tokens.saturating_add(tokens);
        let target = cursor_tokens.div_ceil(per) as usize;
        while blocks.len() < target.min(map_cells) {
            blocks.push(cell);
        }
    };
    place(pinned, Block::Pinned, &mut blocks);
    place(summary_tokens, Block::Summary, &mut blocks);
    place(recent, Block::Recent, &mut blocks);
    while blocks.len() < map_cells {
        blocks.push(Block::Free);
    }
    blocks.truncate(map_cells);
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(pinned: u64, msgs: &[(u64, bool)], recent: u64) -> HistoryShape {
        HistoryShape {
            pinned_tokens: pinned,
            messages: msgs.iter().map(|(t, i)| (*t, *i)).collect(),
            recent_tokens: recent,
        }
    }

    #[test]
    fn exact_block_count_and_pinned_first() {
        let sh = shape(2_000, &[(1_000, false), (500, true), (1_000, false)], 1_000);
        for cells in [80, 240, 391] {
            let b = build_before(4_500, 200_000, &sh, cells, false);
            assert_eq!(b.len(), cells, "exact map size");
            assert_eq!(b[0], Block::Pinned, "pinned always first");
        }
    }

    #[test]
    fn used_fraction_matches_within_one_block() {
        let sh = shape(2_000, &[(10_000, false); 10], 5_000);
        let size = 200_000u64;
        let used = 100_000u64;
        let b = build_before(used, size, &sh, 200, false);
        let occupied = b.iter().filter(|x| **x != Block::Free).count() as f64;
        let per = tokens_per_block(size, 200) as f64;
        let want = used as f64 / per;
        assert!(
            (occupied - want).abs() <= 1.0,
            "occupied {occupied} vs used/per {want}"
        );
    }

    #[test]
    fn after_is_pinned_summary_recent_free() {
        let sh = shape(2_000, &[(10_000, false); 10], 8_000);
        let b = build_after(41_000, 200_000, &sh, 4_000, 200);
        assert_eq!(b[0], Block::Pinned);
        let first_free = b.iter().position(|x| *x == Block::Free).expect("free");
        let head = &b[..first_free];
        // order: all Pinned, then all Summary, then all Recent
        let kinds: Vec<Block> = {
            let mut k = vec![];
            let mut cur = None;
            for x in head {
                if Some(*x) != cur {
                    k.push(*x);
                    cur = Some(*x);
                }
            }
            k
        };
        assert_eq!(kinds, vec![Block::Pinned, Block::Summary, Block::Recent]);
        assert!(b.iter().all(|x| *x != Block::Turn));
    }

    #[test]
    fn prune_marks_old_tool_outputs_only() {
        let sh = shape(
            1_000,
            &[(2_000, false), (2_000, true), (2_000, false)],
            2_000,
        );
        let b = build_before(7_000, 20_000, &sh, 100, true);
        assert!(
            b.contains(&Block::Pruned),
            "the old tool output renders as Pruned (B)"
        );
        // the recent tail is never pruned
        let b2 = build_before(7_000, 20_000, &sh, 100, false);
        assert!(!b2.contains(&Block::Pruned));
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;

    /// К8 property loop (deterministic RNG): for random shapes/sizes the
    /// map is exactly `cells` long, Pinned is first, and the occupied
    /// fraction equals used/size within one block.
    #[test]
    fn property_map_invariants() {
        let mut rng: u64 = 0x1234_5678;
        let next = |rng: &mut u64| {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            *rng
        };
        for case in 0..300 {
            let size = 1 + next(&mut rng) % 400_000;
            let used = 1 + next(&mut rng) % size;
            let msgs: Vec<(u64, bool)> = (0..(next(&mut rng) % 12 + 1))
                .map(|_| {
                    let t = 1 + next(&mut rng) % 20_000;
                    (t, next(&mut rng) % 3 == 0)
                })
                .collect();
            let shape = HistoryShape {
                pinned_tokens: next(&mut rng) % 5_000,
                messages: msgs,
                recent_tokens: next(&mut rng) % 8_000,
            };
            let cells = 40 + (next(&mut rng) % 400) as usize;
            for pruned in [false, true] {
                let b = build_before(used, size, &shape, cells, pruned);
                assert_eq!(b.len(), cells, "case {case}: exact size");
                let pinned_cells = b.iter().filter(|x| **x == Block::Pinned).count();
                assert!(
                    pinned_cells == 0 || b.first().is_some_and(|x| *x == Block::Pinned),
                    "case {case}: pinned either <1 cell or first ({pinned_cells} cells)"
                );
                // pinned never appears after other blocks
                if let Some(first_other) = b.iter().position(|x| *x != Block::Pinned) {
                    if b[first_other..].contains(&Block::Pinned) {
                        panic!(
                            "case {case}: pinned after other blocks: shape={shape:?} used={used} size={size} cells={cells} pruned={pruned} firstblocks={:?}",
                            &b[..10.min(b.len())]
                        );
                    }
                }
                let per = tokens_per_block(size, cells);
                let occupied = b.iter().filter(|x| **x != Block::Free).count() as u64;
                let want = used.div_ceil(per);
                assert!(
                    occupied.abs_diff(want) <= 1,
                    "case {case}: occupied {occupied} vs ceil(used/per) {want} (used={used}, per={per})"
                );
            }
        }
    }
}
