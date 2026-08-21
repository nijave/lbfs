.PHONY: check fmt clippy test build-musl test-loopback vm-up vm-deploy vm-test vm-down

check: fmt clippy test

fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

test-loopback:
	cargo test -p lbfs-tests --test loopback -- --ignored --test-threads=1

build-musl:
	cargo build --release --target x86_64-unknown-linux-musl -p lbfs-server -p lbfs-client

vm-up:
	vm/up.sh $(KERNEL)

vm-deploy: build-musl
	vm/deploy.sh

vm-test:
	vm/test.sh

vm-down:
	vm/down.sh
