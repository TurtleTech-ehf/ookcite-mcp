#!/usr/bin/env bash
# Record an asciinema demo of ookcite-mcp in Claude Code.
# Usage: ./demo/record.sh
# Output: demo/ookcite-demo.cast
#
# This uses asciinema rec with a scripted input to simulate a clean session.
# Since Claude Code is interactive and we can't script it directly, we record
# a manual session. Run this, then type the prompts below naturally.
#
# Prompts to type during recording:
#
#   1. "Validate this DOI: 10.1038/187493a0"
#      (Shows validate_doi, the anti-hallucination use case)
#
#   2. "Format that citation in IEEE style"
#      (Shows format_citation with style selection)
#
#   3. "Is 10.9999/fake-doi a real paper?"
#      (Shows INVALID response -- catches a hallucination)
#
# Tips:
#   - Keep responses visible for 2-3 seconds before next prompt
#   - Terminal should be ~100 cols x 30 rows for clean recording
#   - Use a clean prompt (PS1='$ ')

set -euo pipefail

CAST_FILE="demo/ookcite-demo.cast"
mkdir -p demo

echo "Starting asciinema recording..."
echo "Type the demo prompts inside Claude Code."
echo "Press Ctrl-D or type 'exit' to stop recording."
echo ""

export PS1='$ '
export TERM=xterm-256color

asciinema rec \
  --cols 100 \
  --rows 30 \
  --title "OokCite MCP -- Citation validation for LLMs" \
  "$CAST_FILE"

echo ""
echo "Recording saved to $CAST_FILE"
echo ""
echo "To preview:  asciinema play $CAST_FILE"
echo "To upload:   asciinema upload $CAST_FILE"
echo "To embed:    Use asciinema-player JS on the landing page"
