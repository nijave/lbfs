.PHONY: check fmt clippy test build-musl test-loopback vm-up vm-deploy vm-test vm-down

check: fmt clippy test

fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

# Both halves of spec §10 layer 3: the in-process mount, which can reach the
# connection and count the server's descriptors, and the shipped binary, whose
# argument handling, signal handlers and shutdown ordering only exist in
# `main.rs`. Ignored by default — they need /dev/fuse — so `make test` leaves
# them alone and this target is the only thing that asks for them.
test-loopback:
	cargo test -p lbfs-tests --test loopback -- --ignored --test-threads=1
	cargo test -p lbfs-client --test loopback_cli -- --ignored --test-threads=1

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
