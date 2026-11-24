# Hook Stdin Fixtures (`docs/crate-contracts.md` §3.2 & §10)

41 minimal realistic hook stdin JSON payloads verified against local CLI schemas (`claude 2.1.299`, `codex-cli 0.167.2`, `agy 1.5.13`).

## Expected Normalization Matrix (§3.2)

### `claude/` (16 files, `snake_case`)
| File | `EventKind` | `Confidence` | `subagent_id` | `model` |
|---|---|---|---|---|
| `SessionStart.json` | `SessionStart` | `Exact` | `None` | `Some("claude-opus-5-5")` |
| `UserPromptSubmit.json` | `Working` | `Exact` | `None` | `None` |
| `PermissionRequest.json` | `NeedsPermission` | `Exact` | `None` | `None` |
| `PermissionRequest__subagent.json` | `NeedsPermission` | `Exact` | `Some("sub-claude-01")` | `None` |
| `Notification__permission_prompt.json` | `NeedsPermission` | `Exact` | `None` | `None` |
| `Notification__idle_prompt.json` | `Ignored` | `Exact` | `None` | `None` |
| `Notification__elicitation_dialog.json` | `NeedsInput` | `Exact` | `None` | `None` |
| `Notification__auth_success.json` | `Ignored` | `Exact` | `None` | `None` |
| `Stop.json` | `TurnComplete` | `Exact` | `None` | `None` |
| `StopFailure.json` | `Error` | `Exact` | `None` | `None` |
| `SessionEnd.json` | `SessionEnd` | `Exact` | `None` | `None` |
| `SubagentStart.json` | `SubagentStart` | `Exact` | `Some("sub-claude-01")` | `None` |
| `SubagentStop.json` | `SubagentEnd` | `Exact` | `Some("sub-claude-01")` | `None` |
| `TeammateIdle.json` | `SubagentEnd` | `Exact` | `Some("teammate-01")` | `None` |
| `PreToolUse__subagent.json` | `Ignored` | `Exact` | `Some("sub-claude-01")` | `None` |
| `UnknownEvent.json` | `Ignored` | `Exact` | `None` | `None` |

### `codex/` (12 files, `snake_case`)
| File | `EventKind` | `Confidence` | `subagent_id` | `model` |
|---|---|---|---|---|
| `SessionStart.json` | `SessionStart` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `UserPromptSubmit.json` | `Working` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `PermissionRequest.json` | `NeedsPermission` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `PermissionRequest__subagent.json` | `NeedsPermission` | `Exact` | `Some("sub-codex-01")` | `Some("gpt-6.2-sol")` |
| `PreToolUse.json` | `ToolStart` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `PostToolUse.json` | `ToolEnd` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `Stop.json` | `TurnComplete` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `Interrupt.json` | `Interrupted` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `SessionEnd.json` | `SessionEnd` | `Exact` | `None` | `Some("gpt-6.2-sol")` |
| `SubagentStart.json` | `SubagentStart` | `Exact` | `Some("sub-codex-01")` | `Some("gpt-6.2-sol")` |
| `SubagentStop.json` | `SubagentEnd` | `Exact` | `Some("sub-codex-01")` | `Some("gpt-6.2-sol")` |
| `UnknownEvent.json` | `Ignored` | `Exact` | `None` | `Some("gpt-6.2-sol")` |

### `agy/` (13 files, `camelCase`)
| File | `EventKind` | `Confidence` | `subagent_id` | `model` |
|---|---|---|---|---|
| `SessionStart.json` | `SessionStart` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `PreInvocation.json` | `Working` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `PostInvocation.json` | `Ignored` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `PreToolUse.json` | `ToolStart` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `PostToolUse.json` | `ToolEnd` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__idle.json` | `TurnComplete` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__not_idle.json` | `Working` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__continuation.json` | `Working` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__user_canceled.json` | `Interrupted` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__error.json` | `Error` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__max_invocations.json` | `Error` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `Stop__max_token_budget.json` | `Error` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
| `UnknownEvent.json` | `Ignored` | `Exact` | `None` | `Some("gemini-4-neon-high")` |
