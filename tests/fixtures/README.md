# Test Fixtures Index

This directory contains test fixtures and event sequences used to verify tachograph's ingestion of agent lifecycle hooks, stream-json lines, and multi-turn interleaving.

| Fixture Path | Provider | Event / Category | Source URL / Documentation |
|---|---|---|---|
| `hooks/claude/SessionStart.json` | Claude Code | `SessionStart` | [Claude Code Hooks Docs](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/claude/UserPromptSubmit.json` | Claude Code | `UserPromptSubmit` | [Claude Code Hooks Reference](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/claude/PreToolUse.json` | Claude Code | `PreToolUse` | [Claude Code Hooks Reference](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/claude/PostToolUse.json` | Claude Code | `PostToolUse` | [Claude Code Hooks Reference](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/claude/Stop.json` | Claude Code | `Stop` | [Claude Code Architecture](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/claude/SubagentStop.json` | Claude Code | `SubagentStop` | [Claude Code Subagents](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/subagents) |
| `hooks/claude/PreCompact.json` | Claude Code | `PreCompact` | [Claude Code Compaction](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/context-management) |
| `hooks/claude/SessionEnd.json` | Claude Code | `SessionEnd` | [Claude Code Lifecycle](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/claude/Notification.json` | Claude Code | `Notification` | [Claude Code Notifications](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `hooks/gemini/SessionStart.json` | Gemini CLI | `SessionStart` | [Gemini CLI Documentation](https://geminicli.com/docs/hooks/) |
| `hooks/gemini/BeforeAgent.json` | Gemini CLI | `BeforeAgent` | [Gemini CLI Lifecycle Hooks](https://geminicli.com/docs/hooks/) |
| `hooks/gemini/BeforeTool.json` | Gemini CLI | `BeforeTool` | [Gemini CLI Tool Hooks](https://geminicli.com/docs/hooks/) |
| `hooks/gemini/AfterTool.json` | Gemini CLI | `AfterTool` | [Gemini CLI Tool Hooks](https://geminicli.com/docs/hooks/) |
| `hooks/gemini/AfterAgent.json` | Gemini CLI | `AfterAgent` | [Gemini CLI Lifecycle Hooks](https://geminicli.com/docs/hooks/) |
| `hooks/gemini/SessionEnd.json` | Gemini CLI | `SessionEnd` | [Gemini CLI Lifecycle Hooks](https://geminicli.com/docs/hooks/) |
| `hooks/jetski/SessionStart.json` | Jetski | `SessionStart` | `jetski-customizations/docs/hooks.md` |
| `hooks/jetski/PreInvocation.json` | Jetski | `PreInvocation` | `jetski-customizations/docs/hooks.md` |
| `hooks/jetski/PreToolUse.json` | Jetski | `PreToolUse` | `jetski-customizations/docs/hooks.md` |
| `hooks/jetski/PostToolUse.json` | Jetski | `PostToolUse` | `jetski-customizations/docs/hooks.md` |
| `hooks/jetski/PostInvocation.json` | Jetski | `PostInvocation` | `jetski-customizations/docs/hooks.md` |
| `hooks/jetski/Stop.json` | Jetski | `Stop` | `jetski-customizations/docs/hooks.md` |
| `streams/claude-stream.jsonl` | Claude Code | NDJSON Stream (`init`, `assistant`, `user`, `result`) | [Claude Code Headless Mode](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/headless) |
| `streams/gemini-stream.jsonl` | Gemini CLI | NDJSON Stream (`init`, `message`, `tool_use`, `result`) | [Gemini CLI Headless Mode](https://geminicli.com/docs/output-formats/) |
| `streams/codex-exec.jsonl` | Codex | NDJSON Stream (`thread.started`, `turn.started`, `item.completed`, `turn.completed`) | [OpenAI Codex CLI Reference](https://developers.openai.com/codex/cli) |
| `sequences/claude-parallel-tools.jsonl` | Claude Code | Multi-turn sequence with parallel `PreToolUse` and interleaved `Task` subagent | [Claude Code Subagent Specification](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/hooks) |
| `sequences/jetski-session.jsonl` | Jetski | Full multi-turn session sequence (commands, reads, edits, tests) | `jetski-customizations/docs/hooks.md` |
