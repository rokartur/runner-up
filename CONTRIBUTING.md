# Contributing to runner-up

Thanks for your interest in contributing!

## Getting Started

```sh
git clone https://github.com/rokartur/runner-up.git
cd runner-up
make build
make test
```

## Making Changes

1. Fork the repo and create a branch from `main`.
2. Make your changes.
3. Run `make lint` and `make test` to verify.
4. Open a pull request. CI must pass: `cargo fmt --check`, `cargo clippy`, and
   `cargo test` on Linux, macOS, and Windows, gathered in the `CI passed` check.

## Guidelines

- Keep PRs focused — one feature or fix per PR.
- Follow existing code style (`cargo fmt` and `cargo clippy` are enforced in CI).
- Add tests for new functionality when possible.

## Before a release

release-please opens a release PR, and merging it publishes the release. CI
installs no Tart, so the tests against real Tart skip there and report passed
without running. Before merging a release PR, run them on a Mac with Apple
silicon and [Tart](https://tart.run) installed:

```sh
make test-real-tart
```

It runs every test named `real_tart_...` in `src/cmd/tests/` against a private
Tart store, and fails rather than skips when Tart is missing. Name any new test
against real Tart `real_tart_...` so this check runs it; `tests/ciguard` fails
when one is left out.

## Reporting Issues

Open a [GitHub issue](https://github.com/rokartur/runner-up/issues) with steps to reproduce. Include your OS, architecture, and runner-up version (`runner-up version`).

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
