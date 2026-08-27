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
# Not musl, and not the host toolchain: a distro-packaged rustc has no musl std
# to build against. A container with the guests' own libc family is both
# simpler and closer to what the guests run — Debian's glibc is older than
# Ubuntu 26.04's, which is the direction that works.
#
# The container installs nothing. The io-uring crate issues raw syscalls, so
# liburing is not involved, and since fuser 0.16.0 the client mounts through
# its pure-Rust path — it runs `fusermount3` rather than linking libfuse3, so
# there are no FUSE headers to find at build time and no FUSE library to load
# at run time. The guests still need the `fuse3` package for the `fusermount3`
# binary itself; `vm/lib.sh` asks for it.
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
	  bash -euc 'RUSTUP_TOOLCHAIN=$$(rustup default | cut -d" " -f1) \
	    cargo build --release --target-dir target/guest -p lbfs-server -p lbfs-client'

vm-up:
	vm/up.sh $(KERNEL)

vm-deploy: build-guest
	vm/deploy.sh

vm-test:
	vm/test.sh

vm-down:
	vm/down.sh
