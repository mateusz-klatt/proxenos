# Claude Code print results

## Captured fixtures (captured, not edited)

- **Date**: 2026-11-26
- **CLI version**: `2.1.332 (Claude Code)`
- **Platform**: Linux x86_64
- **Working directory**: fresh empty temporary directories under `/tmp`
- **Model calls spent**: 4 successful model turns total (`captured-start-ok`, `captured-resume-ok`, `captured-canary-start`, and `captured-canary-resume`); all other captures spent 0 model calls.
- **Privacy scan**: Every captured file was scanned for tokens, e-mail addresses, home paths (`/home/...`), hostnames, and account or organization IDs; none appeared. The field `credential_expires_at` present in authenticated responses is an ISO-8601 token expiry timestamp, not a credential. All captured files are stored byte-for-byte (**captured, not edited**).

### Exact invocations

1. `captured-start-ok.stdout.json` (exit code 0, stderr empty; model call 1 of 4):
   ```sh
   claude -p --output-format json --permission-mode plan --model haiku \
     --append-system-prompt "Advise the caller and leave the workspace as you found it." \
     -- "Reply with the single word OK."
   ```
   Observed: invoked with `--model haiku`; `modelUsage` reported `claude-sonnet-5-5` (`total_cost_usd: 0.22713859999999997`, `session_id: 76655224-21a2-4670-925b-2681c62f95a3`).

2. `captured-resume-ok.stdout.json` (exit code 0, stderr empty; model call 2 of 4):
   ```sh
   claude -p --output-format json --permission-mode plan --model haiku \
     --resume 76655224-21a2-4670-925b-2681c62f95a3 \
     -- "Reply with the single word OK."
   ```
   Observed: `modelUsage` and `total_cost_usd` (`0.24204239999999996`) are cumulative across the session (start + reply), while top-level `usage` reflects the resumed turn.

3. `captured-canary-start.stdout.json` (exit code 0, stderr empty; model call 3 of 4):
   ```sh
   claude --print --output-format json --permission-mode plan --disable-slash-commands \
     --model haiku --append-system-prompt "If asked for the codeword, answer ZEBRA-41." \
     -- "Reply with the single word OK."
   ```
   Observed: `result: "OK"`, `session_id: 045dfd54-03fc-4163-8786-08b36ac575f6`, `modelUsage` reported `claude-sonnet-5-5`, `total_cost_usd: 0.19139109999999998`.

4. `captured-canary-resume.stdout.json` (exit code 0, stderr empty; model call 4 of 4):
   ```sh
   claude --print --output-format json --permission-mode plan --disable-slash-commands \
     --model haiku --resume 045dfd54-03fc-4163-8786-08b36ac575f6 \
     -- "What is the codeword? Reply with it only."
   ```
   Observed: resumed without `--append-system-prompt` and returned `result: "ZEBRA-41"` with cumulative `total_cost_usd: 0.2493133` and `modelUsage` reporting `claude-sonnet-5-5`, confirming the appended system prompt persisted across `--resume`.

5. `captured-unknown-model.stdout.json` and `captured-unknown-model.stderr.txt` (exit code 1; 0 model calls):
   ```sh
   claude -p --output-format json --permission-mode plan --model nonexistent-model-xyz \
     -- "Reply with the single word OK."
   ```
   Note: `is_error` is `true` even though `subtype` is `"success"`, with `api_error_status: 404` and `api_error_type: "not_found_error"`.

6. `captured-not-logged-in.stdout.json` (exit code 1, stderr empty; 0 model calls):
   ```sh
   env -u ANTHROPIC_API_KEY claude --bare -p --output-format json --permission-mode plan \
     -- "Reply with the single word OK."
   ```

7. `captured-bogus-resume.stderr.txt` (exit code 1, stdout empty; 0 model calls):
   ```sh
   claude -p --output-format json --permission-mode plan \
     --resume 00000000-0000-4000-8000-000000000000 \
     -- "Reply with the single word OK."
   ```

8. `captured-local-command.stdout.json` (exit code 0, stderr empty; 0 model calls):
   ```sh
   env -u ANTHROPIC_API_KEY claude --bare -p --output-format json --permission-mode plan \
     -- "/cost"
   ```
   Note: demonstrates a zero-turn (`num_turns: 0`, `local_command: "cost"`) local command exit with exit code 0 and `is_error: false`, which Proxenos rejects as a failure because no model turn ran.

## Synthetic fixtures (synthetic, not measured)

These files are **synthetic, not measured**. They follow the JSON result layout measured above to test error paths that cannot be captured without exhausting quota, hitting a turn limit, or triggering tool denials:

- `synthetic-quota-error.stdout.json`: rate-limit / quota failure (`is_error: true`, `api_error_type: "rate_limit_error"`, reset hint in `result`).
- `synthetic-error-max-turns.stdout.json`: non-`"success"` terminal subtype (`subtype: "error_max_turns"`, `is_error: false`, both a partial `result` and an `errors` array so tests assert `errors` takes precedence over `result`, with non-zero usage and cost).
- `synthetic-permission-denial.stdout.json`: empty `result` with a non-empty `permission_denials` list, classified as `refusal`.
