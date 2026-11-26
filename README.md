# Proxenos

Proxenos lets one coding agent consult another. An agent working in Claude Code, Codex CLI,
Antigravity CLI, Kimi Code, Grok CLI, Cursor Agent or GitHub Copilot CLI hands a task to one of
the available providers over MCP and gets the answer back in the same conversation.

In a Greek city, a proxenos was a citizen who looked after the envoys of another city. This one
looks after other agents.

## Install from source

From the repository root, build and install Proxenos with Cargo. With rustup, Cargo uses
Rust 1.100.0 as pinned in `rust-toolchain.toml`. Put Cargo’s bin directory on your `PATH`
(normally `~/.cargo/bin` on Unix or `%USERPROFILE%\.cargo\bin` on Windows).

```sh
cargo install --locked --path . --bin proxenos
proxenos --version
```

## Connect a client

Run the registration command for the client you use, then start a new client session.

| Client | Register | Remove the registration |
| --- | --- | --- |
| Codex CLI | `proxenos integrate codex` | `proxenos integrate codex --remove` |
| Claude Code | `proxenos integrate claude-code` | `proxenos integrate claude-code --remove` |

To check the setup without spending a model turn, run `proxenos doctor --json`. It checks the
registration, probes the provider CLIs and asks the registered server to list its tools. Look
for `codex.handshake` or `claude-code.handshake` in `checks`. On a successful handshake,
`details.tools` lists the tools the server advertised for that client.

For a manual MCP registration, configure the client to launch `proxenos mcp --client <client>`
over standard I/O. The accepted client names are `claude`, `codex`, `antigravity`, `kimi`,
`grok`, `cursor` and `copilot`.

## Delegate a task

The available providers are Claude Code (`claude`), Codex CLI (`codex`), Antigravity
(`antigravity`, using `agy`), Kimi Code (`kimi`) and GitHub Copilot CLI (`copilot`).
Each provider whose CLI is found offers a start tool and a matching `-reply` tool.
The server omits the client’s own provider. Copilot requires `access: "full"`.
For example, Claude Code can call `codex` with a `prompt`, then call `codex-reply` with a
new prompt and the `thread_id` returned by Codex.

Calls wait for a result by default. Set `wait` to `false` to get a background handle, pass
it in the `handles` list to `await`, or pass it as `handle` to `cancel`. An `await` call
waits up to 20 seconds. If the task is still running, call `await` again on the same server.
The [architecture](docs/architecture.md) describes results, provider permissions and session limits.

## Licence

[Apache-2.0](LICENSE).
