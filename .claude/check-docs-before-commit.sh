#!/usr/bin/env bash
# Hook: blocks git commit when crates/*.rs changed but docs-site/ not staged.
# Called by Claude Code PreToolUse hook on Bash tool.

CMD=$(jq -r '.tool_input.command // empty' 2>/dev/null || echo '')

# Only intercept git commit commands (not --no-verify bypasses)
if ! echo "$CMD" | grep -q 'git.*commit'; then
  exit 0
fi
if echo "$CMD" | grep -q -- '--no-verify'; then
  exit 0
fi

cd /Users/cristian/nexusdb || exit 0

SC=$(git diff --cached --name-only 2>/dev/null | grep -E '^crates/.*\.rs$' | wc -l | tr -d ' ')
SD=$(git diff --cached --name-only 2>/dev/null | grep -E '^docs-site/' | wc -l | tr -d ' ')

if [ "${SC:-0}" -gt 0 ] && [ "${SD:-0}" -eq 0 ]; then
  printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"BLOQUEADO: docs-site/ no actualizado. CLAUDE.md exige docs en el mismo commit que los cambios de codigo. Actualiza docs-site/ antes de commitear crates/. Usa --no-verify para saltear."}}'
  exit 0
fi

exit 0
