# Repository conventions

## Commits

One commit, one cause. A code or config change lands together with the tests,
the documentation, and the plan checkboxes it moves — in that same commit, not
a follow-up. Anyone reverting a single commit should get back a consistent
tree, and the failure this prevents is a `main` that describes behaviour the
code does not have.

Split only where the document stands on its own. `docs/benchmarks/` holds
dated records of experiments, including ones whose code never merged; such a
record names the branch and commit holding the arm it measured, and it belongs
in its own commit.

## Documentation

Four kinds, answering four different questions:

| Path | Owns |
|---|---|
| `docs/superpowers/specs/` | The design as it stands today. Amend it when behaviour changes. |
| `docs/superpowers/plans/` | Work still to do, step by step. Tick the boxes as the steps land, and say at the top which tasks remain. |
| `docs/notes/` | One dated analysis — what somebody established on that date. |
| `docs/benchmarks/` | One dated measurement, plus the restore state it left behind. |

Specs and plans track the code, so they go stale and someone has to fix them.
Notes and benchmarks carry a date instead, and a reader takes them as history.

When execution contradicts a plan or a note, append the correction to that
document rather than rewriting the passage it corrects. A plan rewritten to
match what happened teaches nobody what it got wrong. Sections 5 and 9 of
`docs/superpowers/plans/2026-08-22-fuser-two-step-upgrade.md` both work this
way.

The same rule reaches comments in code and CI: one that names a blocker
should name every blocker, or the next person follows it and lands a red
build.

## Gates

| Command | Run it |
|---|---|
| `make check` | Before every commit — fmt, clippy, and the test suites. |
| `make test-loopback` | Before every commit touching the client, the server, or the protocol. It mounts a real filesystem, so it needs `/dev/fuse` and `fusermount3`. |
| `make vm-test` | Before merging anything that changes the mount path, packaging, or deployment. Needs the libvirt guest pair from `make vm-up`, with the current binaries installed by `make vm-deploy` — against a stale deploy it vouches for the wrong build. |

Read the built artifact, not only the source, when a change alters what the
binary links or execs. `ldd` shows what left; `objdump -T` shows what arrived,
and a `.deb`'s `Depends` shows what either one costs the people installing it.
