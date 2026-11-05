# runner-up - Agent Guide

runner-up is a Rust program that runs GitHub Actions jobs on your own machines: one coordinator holds GitHub runner scale set sessions and starts a one-job runner in a free place on a machine, through that machine's agent over SSH.
User-facing behavior: [README.md](README.md). Each test's intent is in the comment above it - read it before changing what the test covers.

## Layout

- `src/main.rs` calls `cmd::main`; `src/cmd.rs` parses the command line, `src/cmd/init.rs` holds `init` and `machine add`, `src/cmd/tests/` the end-to-end tests; `serve` hands off to `serve` in `src/coordinator/serve.rs`
- `src/coordinator/` - the serve loop (`serve.rs`), scale set sessions, placement and the `Service` state split by part (see its doc comment), place runners, the macOS VM count, updating to the setup's release (`program.rs`, `update.rs`)
- `src/agent/` - the agent and its providers (`docker.rs`, `kvm.rs`, `tart.rs`, `container.rs`, `host.rs`, `hyperv.rs`, `account.rs`), the registration and fork-guard launch they all render (`launch.rs`), the fork guard, and the Windows service and job host; the PowerShell and Python scripts they embed sit beside them
- `src/cli.rs` (how every command reads its flags), `src/config.rs` (the setup file), `src/plan.rs` (order rules), `src/release.rs` (release archives and their signature), `src/lock.rs`, `src/proc.rs`, `src/quote.rs`, `src/version.rs`; `src/go/` keeps the Go standard library behavior the setup file, flags and output depend on
- `tests/ciguard/` - tests that guard the CI, release and install files
- `src/testprog.rs` - a test binary copied as `runner-up` runs as the program, so tests that run runner-up need no second build
- Tests give every wait a deadline (`agent::tests::wait_timeout`, `recv_timeout`), so a hang fails with its cause and nothing a test started outlives it

## Commands

- `make build`, `make test` (`cargo test`), `make lint` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`)
- Clippy for Linux and Windows targets before shipping; CI tests on Linux, macOS, and Windows
- `RUNNER_UP_TEST_SHORT=1 cargo test` skips the tests that run runner-up as a program
- `make test-real-tart` on a Mac with Tart before merging a release PR: the real-Tart tests, which CI skips (CONTRIBUTING.md, "Before a release")
- `nix build .#default` after changing `Cargo.toml` or `Cargo.lock`; `package.nix` builds from `Cargo.lock`

## Invariants

- A runner's one-time registration travels to its place only on standard input or a VM's serial port, never in a file, a command line, a container's or VM's settings, or any process's environment but the runner's own, where the runner reads `ACTIONS_RUNNER_INPUT_JITCONFIG` and removes it.
- Every runner starts with the fork guard as its job-started hook unless `allowForkPullRequests` is set.
- A place is busy exactly while some process holds its lock; running jobs outlive a coordinator stop.
- Resource reservations: see README.md, "Setup file" and "Borrowing a virtual Mac".
- The coordinator stores no token; it asks `gh auth token` for each session.
- The Windows service installs only a release whose `checksums.txt` the release signing key signed; its public half is `RELEASE_SIGNING_KEYS` in `src/release.rs` (release.yml reads it there), its private half only in the `release` environment's secret.
- Recorded GitHub payloads in `testdata/` are renamed; keep them free of real repository, account and machine names.

## Windows compatibility

- The agent runs on Windows; the coordinator runs only on Linux and macOS. Every module must still build for Windows.
- Isolate OS-specific code behind `#[cfg(unix)]`/`#[cfg(windows)]` items (see `src/lock.rs`, `src/proc.rs`).
- Command lines run over SSH on Windows must read the same in cmd and PowerShell (`src/quote.rs`).

## CI

- PRs to `main` pass CI, gathered in the `CI passed` job of `ci.yml`. That workflow must never use `paths`/`paths-ignore`, so a branch rule can require it; `tests/ciguard` guards this.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
