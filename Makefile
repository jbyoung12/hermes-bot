.PHONY: build run release clean test lint fmt fmt-check check

-include .env
export

build:
	cargo build

run:
	cargo run

release:
	cargo build --release

clean:
	cargo clean

test:
	cargo test

lint:
	cargo clippy -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

check: fmt-check lint test
