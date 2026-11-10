#!/bin/sh
# DEFRAG demo (bash): fake ACP agent, usage 53k->41k + streamed compaction
cd "$(dirname "$0")"
export AI_COMMANDER_CONFIG_DIR="$TMPDIR/aic-demo"
export AI_COMMANDER_COMPACTION_VIEW=defrag
export FAKE_ACP_COMPACTION=1
./target/release/ai-commander
