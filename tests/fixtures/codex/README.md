# Codex streams

## Captured failures

Codex CLI 0.172.0 produced these streams on Linux x86_64 on 2026-11-26. Each command ran in
an empty working directory with a fresh `HOME` and `CODEX_HOME`, no credentials and closed
stdin. Network connections were disabled. Both commands exited 1 before any inference.
The stub replays the original bytes.

**Missing provider key.** A local custom provider required the unset variable
`PROXENOS_D5_EMPTY_TEST_KEY`. Codex printed a thread ID, a model metadata warning, and
`turn.started` before emitting the missing-key error and `turn.failed`. Its stderr contains
the CLI notice about reading stdin. The files are `captured-missing-provider-key.stdout.jsonl`
(513 bytes) and `captured-missing-provider-key.stderr.txt` (39 bytes).

```sh
codex exec --json --skip-git-repo-check --sandbox read-only --ephemeral --ignore-user-config \
  --color never --model proxenos-d5-nonexistent-model \
  -c 'model_provider="d5-local-measurement"' \
  -c 'model_providers.d5-local-measurement.name="D5 local measurement"' \
  -c 'model_providers.d5-local-measurement.base_url="http://127.0.0.1:9/v1"' \
  -c 'model_providers.d5-local-measurement.env_key="PROXENOS_D5_EMPTY_TEST_KEY"' \
  -c 'model_providers.d5-local-measurement.stream_max_retries=0' \
  -c 'model_providers.d5-local-measurement.request_max_retries=0' 'Reply with OK.'
```

**Unknown session.** No matching rollout existed in the empty profile. Codex emitted no stdout
and wrote its 126-byte diagnostic to `captured-unknown-session.stderr.txt`.

```sh
codex exec resume --json --skip-git-repo-check -c 'sandbox_mode="read-only"' \
  -c 'approval_policy="never"' -o last-message.txt -- 00000000-0000-4000-8000-000000000001 'Reply with OK.'
```

## Synthetic fixtures

`finished.jsonl` and `empty.jsonl` are synthetic. `claude-delegator/server/codex/index.test.js` records the event layout observed with Codex 0.164.0 but assembles its test stream in a stub. It contains no saved byte stream from the CLI. The fixtures here use that layout with independently written content and IDs. Repeated item IDs in `finished.jsonl` exercise the fleet observation that IDs can recur within a thread.

Other failure tests assemble synthetic events with the same layout. The content-filter test uses the diagnostic fragment recorded in the Proxenos fleet notes on 2026-11-24.
