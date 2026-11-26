# Claude Code print results

## Captured fixtures (captured, not edited)

- **Date**: 2026-11-26
- **CLI version**: `2.1.332 (Claude Code)`
- **Platform**: Linux x86_64
- **Working directory**: fresh empty temporary directory under `/tmp`
- **Model calls spent**: 2 successful model turns (`captured-start-ok` and `captured-resume-ok`); all other captures spent 0 model calls.
- **Privacy scan**: Every captured file was scanned for tokens, e-mail addresses, home paths (`/home/...`), hostnames, and account or organization IDs; none appeared. All captured files are stored byte-for-byte (**captured, not edited**).

### Exact invocations

1. `captured-start-ok.stdout.json` (exit code 0, stderr empty; model call 1 of 2):
   ```sh
   claude -p --output-format json --permission-mode plan --model haiku \
     --append-system-prompt "Advise the caller and leave the workspace as you found it." \
     -- "Reply with the single word OK."
   ```
   Note: Claude Code resolved the `haiku` alias to `claude-sonnet-5-5` in `modelUsage` (`total_cost_usd: 0.22713859999999997`, `session_id: 76655224-21a2-4670-925b-2681c62f95a3`).

2. `captured-resume-ok.stdout.json` (exit code 0, stderr empty; model call 2 of 2):
   ```sh
   claude -p --output-format json --permission-mode plan --model haiku \
     --resume 76655224-21a2-4670-925b-2681c62f95a3 \
     -- "Reply with the single word OK."
   ```

3. `captured-unknown-model.stdout.json` and `captured-unknown-model.stderr.txt` (exit code 1; 0 model calls):
   ```sh
   claude -p --output-format json --permission-mode plan --model nonexistent-model-xyz \
     -- "Reply with the single word OK."
   ```
   Note: `is_error` is `true` even though `subtype` is `"success"`, with `api_error_status: 404` and `api_error_type: "not_found_error"`.

4. `captured-not-logged-in.stdout.json` (exit code 1, stderr empty; 0 model calls):
   ```sh
   env -u ANTHROPIC_API_KEY claude --bare -p --output-format json --permission-mode plan \
     -- "Reply with the single word OK."
   ```

5. `captured-bogus-resume.stderr.txt` (exit code 1, stdout empty; 0 model calls):
   ```sh
   claude -p --output-format json --permission-mode plan \
     --resume 00000000-0000-4000-8000-000000000000 \
     -- "Reply with the single word OK."
   ```

6. `captured-local-command.stdout.json` (exit code 0, stderr empty; 0 model calls):
   ```sh
   env -u ANTHROPIC_API_KEY claude --bare -p --output-format json --permission-mode plan \
     -- "/cost"
   ```
   Note: demonstrates a zero-turn (`num_turns: 0`, `local_command: "cost"`) local command exit with exit code 0 and `is_error: false`, which Proxenos rejects as a failure because no model turn ran.

## Synthetic fixtures (synthetic, not measured)

These files are **synthetic, not measured**. They follow the JSON result layout measured above to test error paths that cannot be captured without exhausting quota, hitting a turn limit, or triggering tool denials:

- `synthetic-quota-error.stdout.json`: rate-limit / quota failure (`is_error: true`, `api_error_type: "rate_limit_error"`, reset hint in `result`).
- `synthetic-error-max-turns.stdout.json`: non-`"success"` terminal subtype (`subtype: "error_max_turns"`, `is_error: false`, `errors` array, non-zero usage and cost).
- `synthetic-permission-denial.stdout.json`: empty `result` with a non-empty `permission_denials` list, classified as `refusal`.
