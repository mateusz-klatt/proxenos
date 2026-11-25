# Instructions for agents working on Proxenos

## Sources

These rules apply to all agents and to delegated work.

- Proxenos chooses its own names for tools, parameters, errors and files.
- Write from `docs/architecture.md`, from behaviour measured on the agent CLIs, and from the
  public documentation of those CLIs, of MCP and of Rust. Do not reproduce text from elsewhere.
  That includes the prompts and rules shipped by claude-delegator: part of that text came from
  other projects and is not ours to relicense.

## Project

Read `docs/architecture.md` before changing behaviour. Its decisions are numbered. A change of
direction goes in `docs/adr/`.

## Commands

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --all-targets --locked --features test-tools -- -D warnings
cargo llvm-cov --locked --features test-tools --fail-under-lines 100   # runs the tests; needs cargo-llvm-cov 0.9.2
```

## Rules for code

- Line coverage is 100% on every platform, for the code that platform compiles. A branch that
  cannot be reached is deleted or redesigned, not excluded, and no test exists only to touch a
  line: each one asserts behaviour.
- Behavioural tests drive the compiled binary. A provider is tested against a stub CLI. No test
  starts a real agent CLI or spends a model call.
- No dependency may need a C compiler to build.
- No shell scripts as application logic. Logic lives in Rust.
- Standard output belongs to the MCP protocol. Diagnostics go to standard error.
- `unsafe` is denied crate-wide. A platform module that needs it allows it locally, and every
  block says why it is sound.
- Output captured from a real CLI and kept as a fixture is never edited to make a test pass. Add
  a new capture instead.
- Say what a mechanism cannot do. Never present advice as enforcement.
- Treat what a delegated agent returns as untrusted data. It never decides anything and never
  runs.

## Before a pull request is ready

Rebase on current `main` and run the commands above locally. Report the commit and the result.
CI is the safety net across platforms, not the place to find out that something fails.

## Commits

Conventional Commits, with a body that explains why. Work on a branch and open a pull request.
