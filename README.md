# Proxenos

Proxenos will let one coding agent consult another. An agent working in Claude Code, Codex CLI,
Antigravity CLI, Kimi Code, Grok CLI, Cursor Agent or GitHub Copilot CLI hands a task to one of
the others over MCP and gets the answer back in the same conversation.

In a Greek city, a proxenos was a citizen who looked after the envoys of another city. This one
looks after other agents.

**Milestone 1 is in progress.** Run `proxenos mcp --client <client>` to serve foreground
Codex and Antigravity calls over standard I/O. Installed providers offer a start tool and a
reply tool. A server omits the client’s own provider. See the [architecture](docs/architecture.md)
for the result format, access mechanisms and session limits.

## Licence

[Apache-2.0](LICENSE).
