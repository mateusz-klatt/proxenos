# Cursor CLI print mode

The native evidence for the Cursor provider candidate is from Linux x86_64,
using `cursor-agent` version `2026.11.22-e9042b2` on 2026-11-26.
A successful native start and resume still need to be recorded.

## Authentication failure

The project captured a signed-out invocation with this argv.

```sh
cursor-agent --output-format json --print 'Reply with OK.'
```

It exited with status 1. Both streams are preserved byte for byte.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `captured-missing-login.stdout.txt` | 0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `captured-missing-login.stderr.txt` | 108 | `4de7608ae6736aa85dde5c2817bc1ee0f62662e311cd742bcf09ec6ad788d225` |

Two fresh, signed-out profiles reproduced that status and both stream hashes
with networking disabled at the OS level. Both passed
`--print --output-format json --force` and ended with `-- 'Reply with OK.'`.
One included `--model synthetic-name`. The other supplied
`--resume=00000000-0000-4000-8000-000000000001`. This exercises argument
parsing up to the authentication check. In a separate disposable profile the
native `create-chat` command printed a canonical UUID and a newline before
the 15-second limit expired. It made no model turn.

## Synthetic success fixtures

`synthetic-start.stdout.json` and `synthetic-reply.stdout.json` are hand-authored
inputs to the compiled stub. They exercise interpretation of a complete
success object, its answer and session UUID, and the different start and
reply arguments required by D5. The CLI reference uses a UUID in the result
example. The candidate accepts a canonical UUID, requires an exact match when
resuming and omits the start model on reply. See the
[Cursor JSON output reference](https://cursor.com/docs/cli/reference/output-format)
and [Cursor parameter reference](https://cursor.com/docs/cli/reference/parameters).

Only full access is available in the candidate. Local help describes
`--force` as allowing commands subject to explicit denials. The adapter
passes no workspace trust, sandbox override or MCP auto-approval flag.
Help describes plan and ask modes as read-only. Their effective restrictions
during print-mode execution still need measurement. See
[Cursor headless CLI](https://cursor.com/docs/cli/headless).

## Native acceptance after included quota returns

Use a project-owned authenticated CLI profile with included quota, a new
empty workspace and a separate directory for recordings. Record the CLI
version, exact argv, exit status and unmodified stdout and stderr for both
invocations. Keep the recordings outside the workspace so the second turn
cannot read the first prompt or answer from a file.

1. Choose a fresh random word or token. Start `cursor-agent --print
   --output-format json --force --model <included-model> -- <prompt>` in the
   empty workspace. Ask it to remember the token and reply with just `READY`,
   without calling tools. Check for exit 0, one success object, the expected
   text, and a canonical `session_id`.
2. In the same workspace, run `cursor-agent --print --output-format json
   --force --resume=<session_id> -- <prompt>`. Ask for just the remembered
   token, without including its value or naming files. Require exit 0, one
   success object, the same session UUID and the exact token.
3. Add the recordings as new fixtures and exercise both through the compiled
   MCP server and stub. Record any difference from the documented shape
   before changing the parser. Keep the synthetic fixtures identified as such.

A quota or authentication error leaves this acceptance check pending.
