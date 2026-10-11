.PHONY: check test fmt lint run smoke docs docs-starlight

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
	mdbook build docs

docs-starlight:
	cd starlight && npm ci && npm run build
