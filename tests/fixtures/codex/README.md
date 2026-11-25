# Codex streams

Not measured: Proxenos has not yet captured the real Codex CLI output shape; these fixtures are synthetic, derived from the predecessor's (`claude-delegator`) code. Capturing real Codex CLI output—starting with pre-turn errors that require no model call (for example `codex exec` with an invalid model while logged out), followed by live turn output when available—is the next step.

Both JSONL files are synthetic. `claude-delegator/server/codex/index.test.js` records the event layout observed with Codex 0.164.0 but assembles its test stream in a stub. It contains no saved byte stream from the CLI. The fixtures here use that layout with independently written content and IDs. Repeated item IDs in `finished.jsonl` exercise the fleet observation that IDs can recur within a thread.

The failure tests assemble synthetic events with the same layout. The content-filter test uses the diagnostic fragment recorded in the Proxenos fleet notes on 2026-11-24.
