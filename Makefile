.PHONY: check test fmt lint run smoke docs

check:
	cargo check

test:
	cargo test

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets --all-features -- -D warnings

run:
	cargo run --release

smoke:
	./scripts/smoke.sh

docs:
	cd starlight && npm ci && npm run build
