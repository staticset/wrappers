//! Process-wide MSSQL connection pool (the deferred review item #15).
//!
//! Without it every scan paid a fresh TCP + (optional) TLS + login to SQL
//! Server — a fixed ~100 ms on a LAN bridge and ~300 ms over the customer's
//! WAN, dwarfing the actual query time and dominating dashboards that fire
//! one statement per widget. A PostgreSQL backend process serves a whole
//! client session (postgres_fdw from the Navigator holds one bridge
//! connection for the entire dashboard), so a per-process pool reuses that
//! login across queries for free.
//!
//! Correctness rules:
//! - a connection is returned only after its row stream was **fully**
//!   drained — TDS leaves unread frames behind on early termination, and
//!   reusing such a connection would desynchronize the protocol;
//! - checkout is optimistic (no `SELECT 1` roundtrip): a stale lease that
//!   the server reset fails the very first `query()` call, before any row
//!   was produced, and is retried once on a fresh connection;
//! - the shared runtime and the idle map live in separate statics: scans
//!   `block_on` the runtime while the streaming task returns its connection,
//!   so the map mutex must never be held across `block_on`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// One pool slot per resolved (conn_string, user) identity.
pub(super) type PoolKey = String;

/// The connection [`connect`] produces. tiberius negotiates TLS inside
/// `Client::connect`, so the same concrete type serves encrypted and
/// plaintext transports alike.
pub(super) type Conn = Client<Compat<TcpStream>>;

/// Idle connections kept per key. Statements in one backend run
/// sequentially, so this only needs to cover a cancelled scan's leftover
/// overlapping the next lease — keeping it small bounds the MSSQL sessions
/// a single backend can hold open.
const MAX_IDLE_PER_KEY: usize = 2;

/// A connection unused this long is dropped on the next lease/return.
/// Bridge sessions span minutes (dashboard refreshes); anything the server
/// killed meanwhile is caught by the reconnect-once retry.
const MAX_IDLE_AGE: Duration = Duration::from_secs(300);

struct IdleConn {
    conn: Conn,
    idle_since: Instant,
}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static IDLE: OnceLock<Mutex<HashMap<PoolKey, Vec<IdleConn>>>> = OnceLock::new();

/// The process-wide async runtime (current-thread, like the framework's
/// `create_async_runtime()`): tasks progress only while a scan's `block_on`
/// drives the loop, which is exactly the streaming shape we run.
pub(super) fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mssql_fdw_rq: failed to build the shared tokio runtime")
    })
}

fn idle_map() -> &'static Mutex<HashMap<PoolKey, Vec<IdleConn>>> {
    IDLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the most recently returned connection for `key`, if a live-looking
/// one exists. Expired entries are dropped as a side effect.
pub(super) fn lease(key: &str) -> Option<Conn> {
    // a panic in another scan while holding the lock must not take down
    // every later query: recover the map and carry on
    let mut map = idle_map()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bucket = map.get_mut(key)?;
    // freshest first (LIFO): hot connections stay warm, cold ones age out
    while let Some(idle) = bucket.pop() {
        if idle.idle_since.elapsed() <= MAX_IDLE_AGE {
            return Some(idle.conn);
        }
    }
    None
}

/// Return a fully-drained connection to the pool, evicting expired entries
/// and overflow (oldest first) on the way in.
pub(super) fn release(key: &str, conn: Conn) {
    let mut map = idle_map()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bucket = map.entry(key.to_string()).or_default();
    bucket.retain(|idle| idle.idle_since.elapsed() <= MAX_IDLE_AGE);
    bucket.push(IdleConn {
        conn,
        idle_since: Instant::now(),
    });
    while bucket.len() > MAX_IDLE_PER_KEY {
        bucket.remove(0);
    }
}

/// Open a fresh connection (the only place a login is ever paid for).
pub(super) async fn connect(config: &Config) -> Result<Conn, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    let tcp = tcp.compat_write();
    Client::connect(config.clone(), tcp).await
}
