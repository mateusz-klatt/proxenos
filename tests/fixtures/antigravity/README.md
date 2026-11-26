# Antigravity print results

## Captured fixtures

### `captured-auth-failed.stdout.json`

Captured, not edited (264 bytes, SHA-256 `e7c67c888408868bb307500372a671210a4bf3139707e4236ce3fd5b3e0c7c9c`).

- CLI: `agy` 1.5.18, not logged in
- Host: macOS 26.7.3 `x86_64`
- Argv: `agy --output-format json --disable-slash-commands --print "Reply OK"`
- Working directory: an empty temporary directory
- Streams: stdout redirected to the file and stderr discarded (stderr printed
  `"Authentication required. Please visit the URL to log in:"` followed by an OAuth URL, so it
  was not saved)
- Exit status: exit code 1 after about 60 s
- Date: `2026-11-26T10:57:30Z`
- Captured by: the macOS verifier

## Synthetic fixtures

Not measured: live `agy` `SUCCESS` turn output and print-timeout output have not yet been captured
from a real CLI; the remaining fixtures are synthetic.

- `finished.json` and `timeout.json`: synthetic. The field names and success and error shapes come
  from the measurements recorded in `server/agy/index.js` and its tests at claude-delegator commit
  `56975da34f8a1821ac21950108c9ee031b0149fe`. That checkout contains no raw Antigravity
  print-result fixture. The response text and conversation ID here were written for Proxenos. A
  reported print timeout can still carry a conversation ID.
- `synthetic-error.json`: synthetic. Non-authentication `ERROR` payload shaped after the measured
  `agy` 1.5.18 error JSON with a synthetic error message written for Proxenos.
