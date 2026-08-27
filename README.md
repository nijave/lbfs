# lbfs

lbfs exports a directory from one machine and mounts it on another over the
network. The client turns each FUSE callback into a request on a pipelined
binary protocol; the server runs that request against the exported tree with
io_uring and sends the answer back. Version 1 aims at a LAN or a VPC: one
client per export, and no authentication of any kind. Anyone who can reach the
port and name a path the allowlist covers gets that export, so run the server
only on a network you trust. mTLS is the next milestone.

Long term, lbfs grows toward a single-writer, multi-reader, volatile overlay
filesystem for CI and build systems: build hosts attach shared layers in real
time instead of pulling data down locally before a job runs. The design spec's
"Fast-Follows and Future Work" section holds the roadmap.

## Build

The host build needs a stable Rust toolchain and nothing else. `make check`
runs the standard gate — `cargo fmt --check`, `cargo clippy -D warnings`, and
the workspace tests:

```sh
make check
cargo build --release          # target/release/lbfs-{server,client}
```

`make build-guest` builds both binaries for the VM pair inside a podman
container and leaves them in `target/guest/release`:

```sh
make build-guest               # GUEST_IMAGE=docker.io/library/rust:1-trixie
```

### Why a container and not a musl target

A distro-packaged rustc has no musl std to build against, so a static musl
binary is not on the table on this host. A container running the guests' own
libc family solves that and stays closer to what the guests run. Debian
trixie's glibc is older than Ubuntu 26.04's, which is the direction that works.
Neither binary links anything beyond libc: the `io-uring` crate issues raw
syscalls, and the client mounts through fuser's pure-Rust path, which runs
`fusermount3` as a child process rather than linking `libfuse3.so`. The guests
still install the `fuse3` package, because that is where `fusermount3` comes
from.

## Server

```sh
lbfs-server --config /etc/lbfs.toml
```

`--config` is the only flag. `RUST_LOG` picks the log level and defaults to
`info`; the server logs its whole configuration at startup, plus the address it
bound and the descriptor limit it ended up with.

### Configuration reference

The file is TOML. The server refuses a key it does not know, so a typo stops
startup instead of quietly leaving a default in place.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen` | string | *required* | `host:port` to bind. The project's port is 9423. |
| `allowed_paths` | list of strings | *required* | Globs an attaching client's export path must match. |
| `max_inflight` | integer | `128` | Requests one client may keep outstanding. |
| `max_io_size` | size string | `"1MiB"` | Largest READ or WRITE payload. |
| `fsync` | `"honor"` / `"ignore"` | `"honor"` | Whether `FSYNC` reaches the disk. |

```toml
listen = "0.0.0.0:9423"
allowed_paths = ["/srv/exports/*"]
fsync = "honor"
```

**`allowed_paths` semantics.** The server opens the path the client asked for
with `O_PATH | O_DIRECTORY`, reads the name back from `/proc/self/fd/N`, and
matches the globs against *that* resolved name — never against the string the
client sent. Two consequences follow, and both surprise people:

- A `*` never crosses a `/`. `/srv/exports/*` covers `/srv/exports/data` and
  refuses `/srv/exports/data/sub`. Spell out the depth you mean, or end the
  pattern with `**`.
- A relative pattern such as `exports/*` matches nothing at all, ever. A
  resolved path always starts at `/`, so no relative glob can equal one. Write
  absolute patterns.

**`max_inflight`.** The server clamps this to the range 8–1024 and then takes
whichever of its own number and the client's proposal is smaller. The result
becomes the window both ends account against for the life of the session.

**`max_io_size`.** Accepts a bare byte count, `KiB`, or `MiB`, and must fit in
a `u32`. The server floors it at 4096, because a sub-page ceiling would make
the kernel split every read. It sizes the buffer pool too: the pool holds
`2 × max_inflight` buffers of this size, so raising both raises resident
memory. The negotiated value also becomes the client's FUSE `max_read` and
`max_write`.

**`fsync`.** Under `"honor"` an `FSYNC` or `FSYNCDIR` runs a real
`fsync`/`fdatasync` and durability means durable on the server's disk. Under
`"ignore"` the server answers both at once without touching the disk, and it
also masks `O_SYNC` and `O_DSYNC` out of every open — otherwise a sync-opened
file would bring back the latency the option exists to remove.

> **Read this before choosing `"ignore"`.** It makes the same trade an NFS
> `async` export makes: latency now, crash durability never. A server that
> loses power loses writes it already acknowledged, and the client has no way
> to find out which. Choose it for scratch space and build trees. Do not choose
> it for anything whose loss would cost more than a rebuild.

## Client

```sh
lbfs-client <server:port> <remote-path> <mountpoint> [flags]
```

The remote path must be absolute — the server matches it after resolving it,
so a relative one earns a confusing denial. The client connects, attaches, and
mounts, in that order, so a wrong port, an unexported path, or a version
mismatch prints a plain error instead of leaving an `EIO` mountpoint behind.
`SIGINT` and `SIGTERM` unmount, drain the writeback, and exit.

| Flag | Default | Meaning |
|---|---|---|
| `--attr-timeout <seconds>` | `1.0` | How long the kernel may trust a cached name or attribute. `0` turns both caches off. Fractions are fine. |
| `--allow-other` | off | Let other users on the client machine reach the mount. |
| `--auto-unmount` | off | Ask `fusermount3` to unmount if the process dies uncleanly. Implies `--allow-other`, and needs `user_allow_other` in `/etc/fuse.conf`. |
| `--no-writeback` | off | Write through to the server instead of letting the kernel batch dirty pages. |

The writeback cache stays on by default because coalescing small writes is the
largest single win for build workloads. The flag travels in the handshake: the
server reads an open's flags differently depending on it, so both ends agree
from the start. A kernel that refuses `FUSE_WRITEBACK_CACHE` makes the client
refuse the mount rather than corrupt appends silently — `--no-writeback` is the
honest way to run on such a kernel.

`RUST_LOG` works the same way it does on the server.

## Deployment

### Debian packages

Tagged builds attach `lbfs-server` and `lbfs-client` `.deb`s to the GitHub
release; every other CI run leaves the same pair as a `lbfs-debs` artifact.

```sh
sudo apt install ./lbfs-server_*_amd64.deb    # or ./lbfs-client_*_amd64.deb
```

The server package installs the binary at `/usr/bin/lbfs-server`, the config at
`/etc/lbfs.toml` as a conffile so your edits survive an upgrade, and a unit
that runs as a dedicated `_lbfs` system user its `postinst` creates. It enables
and starts that unit, so **the shipped config binds `127.0.0.1` rather than
`0.0.0.0`** — v1 has no authentication, and a default reaching every interface
would mean installing the package published the export. Two things to do before
the install is useful:

1. Create the export root and give `_lbfs` access to it. The server acts with
   that account's privileges on every request, so an export tree it cannot
   traverse answers `EACCES` and one it owns outright is one a client can
   rewrite at will:

   ```sh
   sudo install -d -o _lbfs -g _lbfs /srv/exports/data
   ```

2. Point `listen` at an address the client can reach, check `allowed_paths`
   covers the root you just made, then `sudo systemctl restart lbfs-server`.

The client package needs no configuration. It depends on `fuse3` for
`fusermount3`, which fuser's pure-Rust mount path runs to mount without
privileges — `--allow-other` additionally wants `user_allow_other` uncommented
in `/etc/fuse.conf`.

The client package wants `libc6 >= 2.39`, which is Ubuntu 24.04 or Debian 13
and newer. That ceiling is the mount path's doing rather than the protocol's:
running `fusermount3` from Rust pulls in the standard library's spawn path,
which imports `pidfd_spawnp`. The symbol is weak and the binary runs on an
older glibc, but `dpkg` reads the version and refuses the install, so an older
target needs a build on an older builder. The server package asks only
for `libc6 >= 2.34`.

### By hand

`vm/lbfs-server.service` is a working unit; copy it to
`/etc/systemd/system/lbfs-server.service`, put the config at `/etc/lbfs.toml`,
and drop the binary at `/usr/local/bin/lbfs-server`. Change its `User=` to
whichever account owns the export — the server acts with that account's
privileges on every request, and a client asks it for whatever it likes.

### Sizing `LimitNOFILE`

This is the one number an operator has to think about. The server keeps one
`O_PATH` descriptor open for every node a client still remembers — not for
every open file. A client that walks a kernel source tree makes the server hold
a descriptor per file and per directory in it, and holds them until the client
sends a `FORGET` or the session ends. That makes the descriptor limit a bound
on the largest tree a client may hold live at once.

The server raises its own soft limit to the hard limit at startup, which covers
whatever launcher starts it. The unit sets the hard limit that raise aims at:

```ini
LimitNOFILE=1048576
```

A million descriptors covers a large build tree with room to spare. The startup
log names the number the server ended up with, and a failed raise logs a
warning saying `EMFILE` may follow.

## Testing

Four test layers, and the commands that drive them, in ascending order of what
each one asks of the host.

| Command | What it runs | Needs |
|---|---|---|
| `make check` | fmt, clippy `-D warnings`, and the workspace tests — protocol round-trips, allowlist matching, the node table, the io_uring executor, the multiplexer, and raw frames over TCP against a real server | nothing beyond the toolchain |
| `make test-loopback` | a real FUSE mount over a real socket on this host, driven through `std::fs`, plus the shipped client binary | `/dev/fuse` and `fusermount3` |
| `make vm-up [KERNEL=…]` | brings up the libvirt guest pair | libvirt and qemu |
| `make vm-deploy` | builds the guest binaries in a container and installs them on the pair | podman, a running pair |
| `make vm-test` | the cross-VM end-to-end suite: every v1 op, fio with `crc32c` verify, a throughput floor, a build workload, and the disconnect drill | a deployed pair |
| `make vm-down` | tears the pair down | — |

The loopback cases carry `#[ignore]`, so `make test` leaves them alone and
`make test-loopback` is the only thing that asks for them. Neither target skips
quietly: a host missing `/dev/fuse` fails and says which piece to install.

## Known limitations

Every entry below names a deliberate v1 choice. They live here so that nobody
has to rediscover them in production.

**Write-only files can answer `EACCES`.** Under the writeback cache the server
promotes `O_WRONLY` to `O_RDWR`, because the kernel computes append offsets
itself and needs the descriptor to read. The xattr path reopens a node through
`/proc` for the same kind of reason. A file at mode `0222`, which grants write
and refuses read, fails both. Ordinary modes never hit this.

**`security.*` and POSIX ACL xattrs pass straight through.** The server sets
whatever the client asks it to set. A server running as root will write
file capabilities on a client's say-so. Until mTLS lands, treat write
access to an export as equal to write access on the server.

**Attach failures leak existence.** `NOT_EXPORTED` and `ATTACH_DENIED` are
different statuses, so anyone who can reach the port can learn which paths
exist on the server by asking for them. mTLS closes this; nothing in v1 does.

**`getlk` and `setlk` answer `ENOSYS`.** `flock` and POSIX record locks stay
inside the client's kernel and never reach the server. With one client per
export that is the correct answer. With two clients it would not be, which is
part of why v1 assumes one.

**`ls -i` can disagree with `stat`.** A `READDIRPLUS` entry reports the
server's node id as `st_ino`, because fuser sends `attr.ino` as the FUSE
nodeid and a wrong nodeid means `ESTALE` on the next operation. Plain
`READDIR` reports the backing filesystem's inode as `d_ino`. The two halves of
a dirent carry different numbers for the same file. A one-line
change to fuser's `DirEntPlusList::push` would let the client set them
independently; until then the node id wins where it must.

**Pre-epoch timestamps come back wrong.** fuser's `time_from_system_time`
mangles a time before 1970 on its way to the kernel. Files dated before the
epoch are rare and the client fixes nothing here.

**`fsync = "ignore"` is a durability trade, not an optimisation.** See the
warning under the configuration reference. The server acknowledges syncs it
never performed.

**A directory listing snapshots at `OPENDIR`.** The server reads the whole
directory once and answers every page from that snapshot, so a listing never
tears — but the snapshot stays in memory until `RELEASEDIR`, and a directory
of a million names costs that memory once per open handle. `rewinddir` replays
the snapshot. A paging cookie the server never handed out answers `EINVAL`.

**A full `FORGET` queue drops lookup counts.** The client batches `FORGET`
frames and counts what it had to drop in `dropped_forgets`. Every dropped
count pins a server node and its descriptor until the session ends, which
shows up as descriptor use that never falls.

**Xattr values above 64 KiB answer `E2BIG`.** A local `getxattr` would have
returned such a value. The bound comes from what a single protocol frame body
may carry.

**`HELLO` may only grow by appending.** A future client that reshapes the
handshake gets a closed connection rather than a `VERSION_MISMATCH`, because
the server cannot decode the message far enough to answer it politely. Add
fields to the end; never reorder or remove.

**Descriptor limits bound the live tree, not the open files.** See
`LimitNOFILE` above. A limit left at the traditional 1024 meets `EMFILE`
partway through a large listing and reports it as an I/O error a long way from
the cause.

**Relative allowlist patterns match nothing.** The server matches globs
against the resolved path, which always begins with `/`. Write absolute
patterns; a relative one silently covers no export.

**The disconnect drill covers a clean stop.** Stopping the server tears the
sessions down at once and the mount answers `EIO` immediately. A `SIGKILL` or
a network partition leaves the socket open until TCP keepalive gives up, which
takes about 25 seconds — 10 seconds idle, then three probes 5 seconds apart.
Requests in flight hang for that long first.

**No reconnection.** A dropped connection fails every in-flight and later
operation with `EIO`. The mount stays present and unmounts cleanly, but it
never comes back; node and handle state is session-scoped on the server, so
honest recovery needs session resumption. That is the first fast-follow.
