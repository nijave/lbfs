.PHONY: check fmt clippy test build-guest test-loopback vm-up vm-deploy vm-test vm-down

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

# Binaries for the VM guests, built in a container rather than here.
#
# Not musl, and not the host toolchain. The client links libfuse3, so a musl
# build would have to bring its own; and a distro-packaged rustc has no musl
# std to build against in the first place. A container with the guests' own
# libc family is both simpler and closer to what the guests actually run:
# Debian's glibc is older than Ubuntu 26.04's, which is the direction that
# works. The io-uring crate issues raw syscalls, so liburing is not involved,
# and libfuse3 is the one shared library the pair needs beyond libc — cloud-init
# installs it on both guests.
#
# The registry cache is a named volume and `target/guest` is inside the mounted
# checkout, so the second build is a rebuild rather than a redownload. SELinux
# labelling is switched off for the mount instead of relabelling the checkout
# out from under the developer.
GUEST_IMAGE ?= docker.io/library/rust:1-trixie

build-guest:
	podman run --rm \
	  --security-opt label=disable \
	  -v "$(CURDIR)":/work \
	  -v lbfs-guest-cargo:/usr/local/cargo/registry \
	  -w /work \
	  $(GUEST_IMAGE) \
	  bash -euc 'apt-get update -qq && \
	    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
	      libfuse3-dev pkg-config >/dev/null && \
	    RUSTUP_TOOLCHAIN=$$(rustup default | cut -d" " -f1) \
	    cargo build --release --target-dir target/guest -p lbfs-server -p lbfs-client'

vm-up:
	vm/up.sh $(KERNEL)

vm-deploy: build-guest
	vm/deploy.sh

vm-test:
	vm/test.sh

vm-down:
	vm/down.sh
