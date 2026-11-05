.PHONY: build test test-real-tart fmt lint install clean

VERSION ?= dev

build:
	RUNNER_UP_VERSION=$(VERSION) cargo build --release
	cp target/release/runner-up runner-up

test:
	cargo test

# The tests against real Tart, which CI skips: run on a Mac with Apple silicon
# and Tart before a release (CONTRIBUTING.md, "Before a release").
test-real-tart:
	RUNNER_UP_REAL_TART=1 cargo test real_tart_ -- --nocapture

fmt:
	cargo fmt

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

install:
	RUNNER_UP_VERSION=$(VERSION) cargo install --locked --path .

clean:
	cargo clean
	rm -f runner-up runner-up.exe
