# Antigravity print results

## Captured fixtures

### `captured-hydrogen-success.stdout.json` and `captured-hydrogen-success.stderr.txt`

Captured, not edited:

- `captured-hydrogen-success.stdout.json`: 280 bytes, SHA-256
  `391e78eb9840669fa1645d96985bd55b398e0b654dee967095405a17e5296a20`
- `captured-hydrogen-success.stderr.txt`: 0 bytes, SHA-256
  `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`

Provenance:

- CLI: `agy` `1.5.18`, model `gemini-4-hydrogen-xhigh`
- Host: Linux `x86_64`
- Argv:
  `agy --output-format json --disable-slash-commands --model gemini-4-hydrogen-xhigh --print-timeout 180s --log-file <log-file> --print "Proxenos native CLI compatibility capture. Reply with exactly PROXENOS_NATIVE_AGY_SUCCESS. Do not use tools, access files, or modify anything."`
- Working directory: an empty temporary directory
- Exit status: exit code `0`, wall time `3.862 s`, `outer_timeout: false` (`duration_seconds`
  `1.669917903`, `num_turns` `1`)
- Date: `2026-11-26T21:18:42.777968+00:00`
- Observed fields: `conversation_id` (`"196f6bf7-3414-4596-ad1d-b8fc8369d24c"`), `status`
  (`"SUCCESS"`), `response` (`"PROXENOS_NATIVE_AGY_SUCCESS\n"` with trailing newline),
  `duration_seconds`, `num_turns`, and `usage` (`input_tokens`, `output_tokens`,
  `thinking_tokens`, `cache_read_tokens`, `total_tokens`). No `model` field was emitted.

### `captured-hydrogen-print-timeout.stdout.json` and `captured-hydrogen-print-timeout.stderr.txt`

Captured, not edited:

- `captured-hydrogen-print-timeout.stdout.json`: 363 bytes, SHA-256
  `b70fc7ab468335f48c988a51dd1786fc26ff4c331f3998e2988309c653036455`
- `captured-hydrogen-print-timeout.stderr.txt`: 345 bytes, SHA-256
  `42b0e78ccaaca35fb9e6017c05be2cae8b4da2d8191fef03f54aa1b185b068be`

Provenance:

- CLI: `agy` `1.5.18`, model `gemini-4-hydrogen-xhigh`
- Host: Linux `x86_64`
- Argv:
  `agy --output-format json --disable-slash-commands --model gemini-4-hydrogen-xhigh --print-timeout 1s --log-file <log-file> --print "Proxenos native CLI compatibility capture. Reply with exactly PROXENOS_NATIVE_AGY_TIMEOUT. Do not use tools, access files, or modify anything."`
- Working directory: an empty temporary directory
- Exit status: exit code `3`, wall time `3.253 s`, `outer_timeout: false` (`duration_seconds`
  `0.175502678`, `num_turns` `1`)
- Date: `2026-11-26T21:18:42.777968+00:00`
- Observed fields: `conversation_id` (`"fa4b0c06-9011-4f6e-8142-056e48d462ff"`), `status`
  (`"ERROR"`), `response` (`""`), `error`
  (`"print timeout after 1s with the turn still in progress; the response is partial"`),
  `duration_seconds`, `num_turns`, `usage` (all zero counters), and `finished_reason`
  (`"print_timeout"`). Stderr records the `[agy]` notice, `error:` line, and `AGY_ERROR:` JSON
  line with `status: "DEADLINE_EXCEEDED"`, `retryable: false`, and `reason: "print_timeout"`.

### `captured-auth-failed.stdout.json`

Captured, not edited (264 bytes, SHA-256 `e7c67c888408868bb307500372a671210a4bf3139707e4236ce3fd5b3e0c7c9c`).

- CLI: `agy` 1.5.18, not logged in
- Host: macOS 26.7.3 `x86_64`
- Argv: `agy --output-format json --disable-slash-commands --print "Reply OK"`
- Working directory: an empty temporary directory
- Streams: stdout redirected to the file and stderr discarded (stderr printed
  `"Authentication required. Please visit the URL to log in:"` followed by an OAuth URL, so it
  was not saved)
- Exit status: exit code 1 after about 60 s (`duration_seconds` in the JSON is still `0` even
  though the run took about 60 s)
- Date: `2026-11-26T10:57:30Z`
- Captured by: the macOS verifier

## Synthetic fixtures and uncaptured cases

Not yet measured from a real `agy` CLI: explicit `--conversation` reply turns (`antigravity-reply`),
non-empty partial `response` text emitted on `finished_reason: "print_timeout"`, quota exhaustion,
command permission refusals, invalid model / other error shapes, and tool-using turns.

- `finished.json` and `timeout.json`: synthetic. The field names and older `"timeout waiting for
  response"` message shape come from the measurements recorded in `server/agy/index.js` and its
  tests at claude-delegator commit `56975da34f8a1821ac21950108c9ee031b0149fe`. That checkout
  contains no raw Antigravity print-result fixture. The response text and conversation ID here were
  written for Proxenos. A reported print timeout can still carry a conversation ID.
