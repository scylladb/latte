.PHONY: fmt
fmt:
	cargo fmt --all

.PHONY: fmt-check
fmt-check:
	cargo fmt --all -- --check

.PHONY: check
check:
	cargo check --all-targets

.PHONY: clippy
clippy:
	RUSTFLAGS=-Dwarnings cargo clippy --all-targets

.PHONY: test
test:
	cargo test

.PHONY: build
build:
	cargo build --examples --benches

.PHONY: clean
clean:
	cargo clean

.PHONY: docker-build
docker-build:
	docker build --target production -t scylladb/latte:latest --compress .

.PHONY: check-alternator
check-alternator:
	cargo check --all-targets --no-default-features --features alternator

.PHONY: clippy-alternator
clippy-alternator:
	RUSTFLAGS=-Dwarnings cargo clippy --all-targets --no-default-features --features alternator

.PHONY: test-alternator
test-alternator:
	cargo test --no-default-features --features alternator

.PHONY: build-alternator
build-alternator:
	cargo build --examples --benches --no-default-features --features alternator