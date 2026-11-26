# GitHub Copilot CLI print mode

All captures were taken on Linux x86_64 with GitHub Copilot CLI 1.0.116 on
2026-11-26, using an empty working directory and disposable profiles. The
compiled-binary tests play the captured bytes through the Proxenos stub.
They do not start Copilot. Other event sequences constructed in the tests
are synthetic.

## Successful start and reply

A deterministic local HTTP fixture served Copilot's documented OpenAI-compatible
local-provider mode. It returned the same short answer via server-sent events
for both requests. Copilot made one request per turn. The child processes could
reach only the fixture's loopback port. No hosted provider, account, token or
model inference was involved. See [GitHub's local provider documentation](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-llm-models).

The CLI inherited `COPILOT_PROVIDER_TYPE=openai`, a loopback
`COPILOT_PROVIDER_BASE_URL` ending in `/v1`, `COPILOT_MODEL=proxenos-local`
and `COPILOT_PROVIDER_WIRE_API=completions`. Both invocations used
`--no-auto-update --no-custom-instructions --disable-builtin-mcps
--disable-client-telemetry --output-format json --allow-all --stream off
--model proxenos-local --reasoning-effort medium`.
The start used `-p "Return the fixed answer."`. The reply added
`--resume=862fd7f3-7ae3-47d6-b775-a11817f3dc50` and used
`-p "Return the fixed answer again."`. Both returned exit status zero and empty stderr.
A separate pair also completed with model and effort omitted from argv.

Only the contiguous stdout suffix beginning with `assistant.turn_start` and
ending at EOF is included from each invocation. Every byte in each suffix is
original, including the final newline. Earlier events contained bundled skill
descriptions and instructions from the CLI. The whole streams were hashed
before selecting these excerpts.

| File | Full stdout bytes | SHA-256 of full stdout | Excerpt bytes |
| --- | ---: | --- | ---: |
| `captured-start-suffix.stdout.jsonl` | 9730 | `d5a263ab2cf363ea87486cc08ff6c24ce795eaef397067222fa11dfdfd94b0b7` | 5664 |
| `captured-reply-suffix.stdout.jsonl` | 5969 | `c66b78ff7c86623a37e10e19d00c15b39cd29b5424e9e4338c632de68e331a03` | 2228 |

The terminal record reports session-level usage, including premium requests.
The two captures do not establish the token counts, prices or paid usage of a
GitHub-hosted turn.

## Startup failures

Both failures used a fresh signed-out profile, denied network syscalls and
`--no-auto-update --no-custom-instructions --disable-builtin-mcps
--output-format json -p "Reply with OK."`. Both exited with status one before
contacting a provider. Each included file is its entire original stream.

- `captured-missing-login.stdout.jsonl` and `captured-missing-login.stderr.txt`
  are the start. Copilot emitted a session UUID and a failing result even though
  it had no authentication information. Its reported premium requests and API
  duration were both zero.
- `captured-unknown-session.stderr.txt` is the reply with
  `--resume=00000000-0000-4000-8000-000000000001`. stdout was empty.

Each fixture was checked for credentials, personal paths, account e-mail
addresses and sign-in links before inclusion.
