# Codex streams

Both JSONL files are synthetic. `claude-delegator/server/codex/index.test.js` records the event layout observed with Codex 0.164.0 but assembles its test stream in a stub. It contains no saved byte stream from the CLI. The fixtures here use that layout with independently written content and IDs. Repeated item IDs in `finished.jsonl` exercise the fleet observation that IDs can recur within a thread.

The failure tests assemble synthetic events with the same layout. The content-filter test uses the diagnostic fragment recorded in the Proxenos fleet notes on 2026-11-24.
