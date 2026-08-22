//! `lbfs-bench` — load against the wire protocol with no FUSE underneath it.
//!
//! The mount's numbers are the sum of three things: what the kernel's FUSE
//! layer costs, what this client's bridge costs, and what the protocol and the
//! server cost. Only the third is measurable here, and that is the point. A
//! `READ` issued from this binary walks the same [`Connection`] the bridge
//! walks — same window, same frames, same server — while never touching
//! `/dev/fuse`. The difference between a run here and the matching `fio` job
//! through a mount is the FUSE round trip and nothing else.
//!
//! It is a measuring tool, not a test: it writes to a file under the export
//! root, leaves it there for the next run to reuse, and reports one line.

#![deny(unsafe_code)]

use std::error::Error;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use lbfs_client::conn::Connection;
use lbfs_proto::types::{Fh, NodeId, ROOT_NODE};
use lbfs_proto::Errno;
use tokio::task::JoinSet;

/// The protocol's own ceiling on one I/O. A larger block would be refused by
/// the multiplexer before it reached the wire, so it is refused here instead,
/// where the message can say which flag was wrong.
const MAX_BS: u32 = 1 << 20;

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Op {
    Read,
    Write,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Pattern {
    Seq,
    Rand,
}

#[derive(Parser)]
#[command(name = "lbfs-bench", about = "Drive an lbfs server without a mount")]
struct Cli {
    /// The server to attach to, as `host:port`.
    #[arg(long)]
    addr: String,

    /// The exported path, absolute, as the server sees it.
    #[arg(long)]
    export: PathBuf,

    #[arg(long, value_enum)]
    op: Op,

    /// Bytes per operation. Bounded by the negotiated I/O ceiling.
    #[arg(long, default_value_t = 1 << 20)]
    bs: u32,

    /// How many operations to keep in flight. Bounded by the negotiated
    /// window, because a request past it waits on a permit rather than on the
    /// server, and the latency it reports would be a queue length.
    #[arg(long, default_value_t = 1)]
    qd: usize,

    #[arg(long, value_enum, default_value_t = Pattern::Seq)]
    pattern: Pattern,

    #[arg(long, default_value_t = 10)]
    duration: u64,

    /// How large the file is made before the run starts.
    #[arg(long, default_value_t = 512 << 20)]
    size: u64,

    #[arg(long, default_value = "lbfs-bench.dat")]
    file: String,

    /// Seed for the random pattern. Fixed by default so two runs address the
    /// same blocks in the same order.
    #[arg(long, default_value_t = 0x2545_f491_4f6c_dd1d)]
    seed: u64,
}

fn main() -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("lbfs-bench: starting the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(Cli::parse())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lbfs-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    if cli.bs == 0 || cli.bs > MAX_BS {
        return Err(format!("--bs must be between 1 and {MAX_BS}").into());
    }
    if cli.qd == 0 {
        return Err("--qd must be at least 1".into());
    }
    if cli.size < u64::from(cli.bs) {
        return Err("--size must be at least one block".into());
    }
    let addr: SocketAddr = cli
        .addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| format!("{}: no address resolved", cli.addr))?;

    // `writeback = false` because there is no page cache in this picture at
    // all. The flag tells the server whose kernel owns the file size, and the
    // honest answer for a client with no kernel in it is "not mine".
    let (conn, limits, _root) =
        Connection::connect(addr, cli.export.as_os_str().as_bytes(), false).await?;
    if cli.bs > limits.max_io_size {
        return Err(format!(
            "--bs {} is over the negotiated {}",
            cli.bs, limits.max_io_size
        )
        .into());
    }
    if cli.qd > limits.max_inflight as usize {
        return Err(format!(
            "--qd {} is over the negotiated window {}",
            cli.qd, limits.max_inflight
        )
        .into());
    }

    let (node, fh) = open_target(&conn, cli.file.as_bytes()).await?;
    fill(&conn, node, fh, cli.size, limits.max_io_size).await?;

    let report = drive(&conn, node, fh, &cli).await?;

    // Releasing matters even for a tool: the server holds a descriptor per
    // open handle, and a run that walked away from it would charge the next
    // one a slot it never used.
    let _ = conn.release(node, fh).await;
    println!("{report}");
    Ok(())
}

/// The file the run addresses: reused when it is there, created when it is
/// not. `ENOENT` is the only error worth translating into a `CREATE`; anything
/// else is a real failure and reads better before the run than during it.
async fn open_target(conn: &Arc<Connection>, name: &[u8]) -> Result<(NodeId, Fh), Box<dyn Error>> {
    let flags = libc::O_RDWR as u32;
    match conn.lookup(ROOT_NODE, name).await {
        Ok(entry) => {
            let fh = conn.open(entry.node, flags).await.map_err(failed("OPEN"))?;
            Ok((entry.node, fh))
        }
        Err(Errno::ENOENT) => {
            let (entry, fh) = conn
                .create(ROOT_NODE, name, 0o644, flags)
                .await
                .map_err(failed("CREATE"))?;
            Ok((entry.node, fh))
        }
        Err(e) => Err(failed("LOOKUP")(e)),
    }
}

/// Grow the file to `size` with sequential writes.
///
/// A read run against a short file would measure the server answering `EOF`,
/// and a random write run past the end would measure allocation. Both are real
/// costs and neither is the one this tool is pointed at, so the file is made
/// whole first — once, since the next run finds it already grown.
async fn fill(
    conn: &Arc<Connection>,
    node: NodeId,
    fh: Fh,
    size: u64,
    max_io: u32,
) -> Result<(), Box<dyn Error>> {
    let attr = conn
        .getattr(node, Some(fh))
        .await
        .map_err(failed("GETATTR"))?;
    if attr.size >= size {
        return Ok(());
    }
    let chunk = max_io.min(MAX_BS);
    let block = Arc::new(vec![0x5au8; chunk as usize]);
    let mut offset = attr.size / u64::from(chunk) * u64::from(chunk);
    // Pipelined for the same reason the load loop is: filling 512 MiB one
    // synchronous megabyte at a time costs more wall clock than the run.
    let mut set: JoinSet<Result<(), Errno>> = JoinSet::new();
    while offset < size || !set.is_empty() {
        while offset < size && set.len() < 16 {
            let want = chunk.min((size - offset) as u32);
            let (conn, block, at) = (Arc::clone(conn), Arc::clone(&block), offset);
            set.spawn(async move {
                conn.write(node, fh, at, block[..want as usize].to_vec())
                    .await
                    .map(|_| ())
            });
            offset += u64::from(want);
        }
        if let Some(joined) = set.join_next().await {
            joined?.map_err(failed("WRITE while filling"))?;
        }
    }
    Ok(())
}

/// The run itself: keep `qd` operations in flight until the clock runs out,
/// then drain what is still outstanding.
async fn drive(
    conn: &Arc<Connection>,
    node: NodeId,
    fh: Fh,
    cli: &Cli,
) -> Result<String, Box<dyn Error>> {
    let block = Arc::new(vec![0xa5u8; cli.bs as usize]);
    let mut offsets = Offsets::new(cli.pattern, cli.size, cli.bs, cli.seed);
    let mut set: JoinSet<Result<u64, Errno>> = JoinSet::new();
    let mut latencies: Vec<u64> = Vec::new();

    let started = Instant::now();
    let deadline = started + Duration::from_secs(cli.duration);
    loop {
        while set.len() < cli.qd && Instant::now() < deadline {
            let (conn, block) = (Arc::clone(conn), Arc::clone(&block));
            let (op, bs, at) = (cli.op, cli.bs, offsets.next());
            set.spawn(async move {
                let started = Instant::now();
                match op {
                    Op::Read => {
                        conn.read(node, fh, at, bs).await?;
                    }
                    Op::Write => {
                        // The copy is not an artefact of the tool: the bridge
                        // hands the multiplexer an owned buffer per write too,
                        // because a frame outlives the callback that made it.
                        conn.write(node, fh, at, block.as_ref().clone()).await?;
                    }
                }
                Ok(started.elapsed().as_nanos() as u64)
            });
        }
        match set.join_next().await {
            Some(joined) => latencies.push(joined?.map_err(failed("I/O"))?),
            None => break,
        }
    }
    let elapsed = started.elapsed().as_secs_f64();

    if latencies.is_empty() {
        return Err("the run completed no operations".into());
    }
    latencies.sort_unstable();
    let ops = latencies.len() as f64;
    let total: u64 = latencies.iter().sum();
    let bytes = ops * f64::from(cli.bs);
    Ok(format!(
        "op={} bs={} qd={} pattern={} ops={} secs={:.2} iops={:.0} mb_s={:.1} \
         mean_us={:.1} p50_us={:.1} p99_us={:.1}",
        match cli.op {
            Op::Read => "read",
            Op::Write => "write",
        },
        cli.bs,
        cli.qd,
        match cli.pattern {
            Pattern::Seq => "seq",
            Pattern::Rand => "rand",
        },
        latencies.len(),
        elapsed,
        ops / elapsed,
        bytes / elapsed / (1024.0 * 1024.0),
        total as f64 / ops / 1000.0,
        percentile(&latencies, 50) as f64 / 1000.0,
        percentile(&latencies, 99) as f64 / 1000.0,
    ))
}

/// Nearest-rank, on an already sorted slice.
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    let rank = (sorted.len() * pct).div_ceil(100).max(1);
    sorted[(rank - 1).min(sorted.len() - 1)]
}

/// Where the next operation lands.
struct Offsets {
    pattern: Pattern,
    blocks: u64,
    bs: u64,
    cursor: u64,
    state: u64,
}

impl Offsets {
    fn new(pattern: Pattern, size: u64, bs: u32, seed: u64) -> Offsets {
        Offsets {
            pattern,
            // Only whole blocks, so no operation runs off the end of the file.
            blocks: (size / u64::from(bs)).max(1),
            bs: u64::from(bs),
            cursor: 0,
            // Zero is xorshift's fixed point: it would return zero for ever
            // and every "random" offset would be block nought.
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next(&mut self) -> u64 {
        let block = match self.pattern {
            Pattern::Seq => {
                let block = self.cursor % self.blocks;
                self.cursor += 1;
                block
            }
            // xorshift64, Marsaglia's 13/7/17 triple. Not a good generator;
            // good enough to defeat readahead, which is all a block address
            // has to do.
            Pattern::Rand => {
                self.state ^= self.state << 13;
                self.state ^= self.state >> 7;
                self.state ^= self.state << 17;
                self.state % self.blocks
            }
        };
        block * self.bs
    }
}

/// An errno with the operation that produced it, since a bare number does not
/// say whether the mount arrangement or the run itself was wrong.
fn failed(what: &'static str) -> impl Fn(Errno) -> Box<dyn Error> {
    move |e| {
        let reason = std::io::Error::from_raw_os_error(i32::from(e.0));
        format!("{what} failed: {reason}").into()
    }
}
