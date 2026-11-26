# Grok Build headless output

These files preserve complete streams from Grok Build 1.0.92 (b06756fd6256)
on Linux x86_64, 2026-11-26. Tests replay them through the compiled Proxenos
stub. Other JSON assembled in the tests is synthetic.

## Local model start and resume

A fresh profile selected a custom model named `proxenos-local` through
`[models].default`. Its `[model.proxenos-local]` entry used the
`chat_completions` API backend and a loopback `base_url`. The model's
`env_key` named a disposable credential issued and checked by the local
fixture alone. Grok had neither an xAI API key nor a login. Every successful
outbound connection from Grok went to the local fixture; it returned fixed
protocol responses without running inference. See the [Grok Build settings
reference](https://docs.x.ai/build/settings) and [headless CLI
reference](https://docs.x.ai/build/cli/headless-scripting).

| File | Native arguments following `--output-format json --always-approve` | Exit | stdout / stderr bytes |
| --- | --- | ---: | --- |
| `captured-start.stdout.json` | `-p "Return the fixed answer."` | 0 | 626 / 0 |
| `captured-reply.stdout.json` | `--resume 01a21907-cf8c-7550-ad5c-dadebae7ae4b -p "Return the fixed answer again."` | 0 | 626 / 0 |
| `captured-unknown-session.*` | `--resume 00000000-0000-4000-8000-000000000001 -p "Return the fixed answer again."` | 1 | 217 / 195 |

Both successes name the same session UUID and report one completed turn.
The fixture supplied 10 input and 2 output tokens during the first invocation,
then 20 input and 4 output tokens during the second. The reply reported the
second invocation's counts, including one call under `modelUsage`. Each
invocation also made ancillary local requests, so the reported model-call
count is not a count of all connections. The captures contain no price.
The recorded `cwd` identifies a generated temporary directory.

A separate pair passed the model explicitly with `--model proxenos-local`
and `--reasoning-effort medium`. Both completed. Grok warned that this local
model had no effort levels and ignored the requested effort. Which efforts
take effect depends on the model.

The absent UUID produced a structured error and no additional requests to the
local endpoint. The CLI qualified its result by saying it could not search
the remote session registry without a live xAI login.

## Signed-out default provider

`captured-missing-login.stdout.json` and `captured-missing-login.stderr.txt`
came from a separate empty profile with all network connections blocked.
The command was `grok --output-format json --no-memory --no-subagents
-p "Reply with OK."`. It exited 1 with 220 bytes on stdout and 195 on stderr.

All six files were compared to the original streams and inspected for
credentials, account identifiers, sign-in links and private instructions.
The captures exercise a genuine Grok CLI and a local fixture; they do not
measure hosted account quotas, pricing or a paid Grok model.
