# klartext-mcp (M4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A read-only stdio MCP server (`klartext-mcp`) that exposes klartext's diagnostic reads as tools so an AI client can connect to a BMW F-series car over HSFZ and reason about its faults and live data.

**Architecture:** A new lib+bin crate `mcp/` wrapping `klartext-client` (connection/session/keepalive) and `klartext-semantic` (decoding) behind five `rmcp` tools. Server state holds one ephemeral car connection (`Arc<Mutex<Option<Connection>>>`) established by `connect` and torn down by `disconnect`; reads reconnect to the requested ECU's address when the held target differs. ECU targeting is DB-driven (general, multi-car) with a small built-in BMW-wide alias fallback.

**Tech Stack:** Rust 2024, tokio, rmcp 1.8 (`#[tool_router]`/`#[tool]`/`#[tool_handler]`, `Json<T>`, `serve(stdio())`), serde + `rmcp::schemars`, rusqlite (via klartext-semantic), tracing → stderr.

## Global Constraints

- **READ-ONLY surface.** Define only `connect`, `read_faults`, `read_data`, `list_ecus`, `disconnect`. No clear-DTC, actuation, coding, or any write tool — ever. A test asserts the advertised set is exactly these five.
- **stdout is JSON-RPC only.** No `println!`/`print!`/`dbg!`/`eprintln!`-to-stdout anywhere. ALL logging via `tracing` to **stderr**: `tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false)`.
- **Do NOT connect on startup.** The car may be unplugged. Connect only via the `connect` tool.
- **BYO-data.** ISTA SQLiteDB path via env/arg (`KLARTEXT_SEMANTIC_DB`), opened **read-only**. Never embed/commit DB contents. Reads degrade to raw codes when the DB is absent.
- **Reuse M1–M3.** Only additive change allowed: `Catalog::ecus()` in `klartext-semantic`. No protocol/decoding changes, no signature changes to existing items.
- **Dependencies via `cargo add`** (never hand-edit `Cargo.toml` deps). House style: workspace-inherit `tokio`/`anyhow`/`clap`; pin crate-specific deps (`rmcp`, `serde`, `serde_json`, `tracing`, `tracing-subscriber`, `tempfile`, `rusqlite`) directly, mirroring `crates/semantic` and `crates/hsfz`.
- **Edition 2024, latest stable Rust.** `thiserror` for libraries, `anyhow` at the binary boundary.
- **Done = clean:** `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` all clean. Conventional commits. Work on branch `milestone-4-mcp`.

## File Structure

```
crates/semantic/src/catalog.rs   MODIFY  add EcuEntry + Catalog::ecus()
crates/semantic/src/lib.rs       MODIFY  export EcuEntry
mcp/Cargo.toml                   CREATE  via cargo new + cargo add
mcp/src/lib.rs                   CREATE  pub modules + re-export KlartextServer (enables integration tests)
mcp/src/main.rs                  CREATE  thin bin: stderr tracing + parse args + serve(stdio())
mcp/src/config.rs                CREATE  ServerConfig (clap) + client_config()/discovery_wait()
mcp/src/dto.rs                   CREATE  serde+JsonSchema request/response types (grown per task)
mcp/src/ecu.rs                   CREATE  builtin aliases + resolve() + list()
mcp/src/session.rs               CREATE  Connection/SessionState + establish()/ensure_target()
mcp/src/server.rs                CREATE  KlartextServer + 5 tools + ServerHandler
mcp/tests/integration.rs         CREATE  mock-gateway flows + tool-surface + not-connected
```

`mcp/` is a **lib + bin**: `src/lib.rs` exposes the modules (so `tests/integration.rs` can `use klartext_mcp::...` and call the `pub` tool methods); `src/main.rs` is a thin binary over the lib. The binary name is `klartext-mcp` (the package name).

---

### Task 1: `Catalog::ecus()` in klartext-semantic

Adds the read-only ECU-map query that powers DB-driven `list_ecus`/resolution. Self-contained in the semantic crate.

**Files:**
- Modify: `crates/semantic/src/catalog.rs`
- Modify: `crates/semantic/src/lib.rs`
- Test: `crates/semantic/src/catalog.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `klartext_semantic::EcuEntry { pub address: u8, pub group_name: String }` and `Catalog::ecus(&self) -> Result<Vec<EcuEntry>, SemanticError>` (distinct `(address, group_name)`, ordered by address).

- [ ] **Step 1: Add ECU rows to the test fixture.** In `crates/semantic/src/catalog.rs`, in the `tests` module's `fixture()` `execute_batch`, append these INSERTs after the existing `dtc` inserts (the `ecu` table is already created there):

```rust
             INSERT INTO ecu VALUES (16,'zgw_x','d_0010');
             INSERT INTO ecu VALUES (18,'dme_x','d_0012');
             INSERT INTO ecu VALUES (64,'fem_20','d_0040');
             INSERT INTO ecu VALUES (64,'fem_21','d_0040');
```

- [ ] **Step 2: Write the failing test.** Add to the `tests` module:

```rust
    #[test]
    fn ecus_lists_distinct_addresses_ordered() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let ecus = cat.ecus().unwrap();
        assert_eq!(
            ecus,
            vec![
                EcuEntry { address: 16, group_name: "d_0010".to_string() },
                EcuEntry { address: 18, group_name: "d_0012".to_string() },
                EcuEntry { address: 64, group_name: "d_0040".to_string() },
            ]
        );
    }
```

- [ ] **Step 3: Run the test, expect failure.**

Run: `cargo test -p klartext-semantic ecus_lists_distinct_addresses_ordered`
Expected: FAIL — `EcuEntry`/`ecus` not found.

- [ ] **Step 4: Implement `EcuEntry` + `Catalog::ecus()`.** In `crates/semantic/src/catalog.rs`, add the struct after `DtcDescription`:

```rust
/// One ECU slot in the semantic DB: a diagnostic address and its ISTA group name.
///
/// Sourced from ISTA's `XEP_ECUVARIANTS ⋈ XEP_ECUGROUPS` — the general BMW ECU
/// model, not specific to one car.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuEntry {
    /// The diagnostic address (e.g. 0x12 for the DME).
    pub address: u8,
    /// The ISTA group name, e.g. "d_0012".
    pub group_name: String,
}
```

and this method inside `impl Catalog` (after `describe_dtc`):

```rust
    /// List the distinct ECU slots (diagnostic address + ISTA group name) known to
    /// the DB, ordered by address. An empty DB yields an empty list.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn ecus(&self) -> Result<Vec<EcuEntry>, SemanticError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT address, group_name FROM ecu ORDER BY address")?;
        let rows = stmt.query_map([], |row| {
            Ok(EcuEntry {
                address: row.get(0)?,
                group_name: row.get(1)?,
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }
```

- [ ] **Step 5: Export `EcuEntry`.** In `crates/semantic/src/lib.rs`, change the re-export line to:

```rust
pub use catalog::{Catalog, DtcDescription, EcuEntry, SemanticError};
```

- [ ] **Step 6: Run the test, expect pass.**

Run: `cargo test -p klartext-semantic ecus_lists_distinct_addresses_ordered`
Expected: PASS. Also run `cargo test -p klartext-semantic` — all existing tests still pass.

- [ ] **Step 7: Lint + commit.**

```bash
cargo fmt -p klartext-semantic
cargo clippy -p klartext-semantic --all-targets -- -D warnings
git add crates/semantic/src/catalog.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): add Catalog::ecus() for the general ECU map"
```

---

### Task 2: Scaffold `klartext-mcp` (lib+bin) + config + the `disconnect` tool

De-risks the whole rmcp wiring (server struct, router, handler, stdio, stderr tracing, lib+bin test harness) in one compilable slice, ending with a working no-car tool.

**Files:**
- Create: `mcp/Cargo.toml` (via `cargo new` + `cargo add`)
- Create: `mcp/src/lib.rs`, `mcp/src/main.rs`, `mcp/src/config.rs`, `mcp/src/dto.rs`, `mcp/src/server.rs`
- Test: `mcp/tests/integration.rs`

**Interfaces:**
- Consumes: `klartext_client::{ClientConfig, DEFAULT_BROADCAST}`, `klartext_hsfz::{CONNECT_TIMEOUT_DEFAULT_MS, DIAG_PORT, TESTER_ADDRESS}`, `klartext_uds::P2_STAR_SERVER_MAX_DEFAULT_MS`.
- Produces: `klartext_mcp::config::ServerConfig` (clap `Parser`, all fields `pub`) with `client_config(&self, ecu: u8) -> ClientConfig` and `discovery_wait(&self) -> Duration`; `klartext_mcp::server::KlartextServer` with `new(ServerConfig)`, `advertised_tools(&self) -> Vec<String>`, and `pub async fn disconnect(&self) -> Result<Json<DisconnectResult>, McpError>`; `klartext_mcp::dto::DisconnectResult { was_connected: bool }`.

- [ ] **Step 1: Create the crate.**

Run: `cargo new mcp --name klartext-mcp`
Expected: creates `mcp/Cargo.toml` + `mcp/src/main.rs`, and prints that `mcp` was added to the workspace `members`. Verify with `git diff Cargo.toml` — it should now list `"mcp"`. If cargo did *not* add it, add `"mcp"` to the `members` array in the root `Cargo.toml` (membership is structural, not a dependency).

- [ ] **Step 2: Add dependencies via cargo add.**

```bash
cargo add klartext-client --path crates/client -p klartext-mcp
cargo add klartext-semantic --path crates/semantic -p klartext-mcp
cargo add klartext-hsfz --path crates/hsfz -p klartext-mcp
cargo add klartext-uds --path crates/uds -p klartext-mcp
cargo add tokio -p klartext-mcp --features macros,rt-multi-thread,io-std,sync,time
cargo add anyhow -p klartext-mcp
cargo add clap -p klartext-mcp --features derive,env
cargo add rmcp@1 -p klartext-mcp --features server,macros,transport-io
cargo add serde -p klartext-mcp --features derive
cargo add serde_json -p klartext-mcp
cargo add tracing -p klartext-mcp
cargo add tracing-subscriber -p klartext-mcp --features env-filter
cargo add tempfile -p klartext-mcp --dev
cargo add rusqlite -p klartext-mcp --dev --features bundled
```

Verify `mcp/Cargo.toml` resembles (house style — `tokio`/`anyhow`/`clap` should show `workspace = true`; if cargo pinned a version instead, switch those three to `{ workspace = true, features = [...] }`):

```toml
[dependencies]
anyhow.workspace = true
clap = { workspace = true, features = ["derive", "env"] }
klartext-client = { path = "../crates/client" }
klartext-hsfz = { path = "../crates/hsfz" }
klartext-semantic = { path = "../crates/semantic" }
klartext-uds = { path = "../crates/uds" }
rmcp = { version = "1", features = ["server", "macros", "transport-io"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "io-std", "sync", "time"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[dev-dependencies]
rusqlite = { version = "0.40", features = ["bundled"] }
tempfile = "3"
```

- [ ] **Step 3: Write `mcp/src/dto.rs`** (starts with just `DisconnectResult`; later tasks add more):

```rust
//! Wire types for the MCP tools: structured, AI-facing request/response shapes.
//!
//! `schemars` comes from rmcp's re-export so its version always matches rmcp's.

use rmcp::schemars;
use serde::Serialize;

/// Result of `disconnect`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DisconnectResult {
    /// Whether a live connection was dropped (false if already disconnected).
    pub was_connected: bool,
}
```

- [ ] **Step 4: Write `mcp/src/config.rs`:**

```rust
//! Server configuration from CLI args + environment (read-only; mirrors the CLI).

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use klartext_client::{ClientConfig, DEFAULT_BROADCAST};
use klartext_hsfz::{CONNECT_TIMEOUT_DEFAULT_MS, DIAG_PORT, TESTER_ADDRESS};
use klartext_uds::P2_STAR_SERVER_MAX_DEFAULT_MS;

/// klartext MCP server configuration. The car connection is established lazily by
/// the `connect` tool — never at startup.
#[derive(Debug, Clone, Parser)]
#[command(version, about = "Read-only BMW F-series diagnostics as MCP tools over stdio.")]
pub struct ServerConfig {
    /// Default gateway IP for `connect` to use directly (skips discovery). The
    /// `connect` tool's argument overrides this.
    #[arg(long, env = "KLARTEXT_GATEWAY")]
    pub gateway_ip: Option<IpAddr>,

    /// Bind discovery to this link-local source IP (default: auto-detect).
    #[arg(long, env = "KLARTEXT_BIND")]
    pub bind: Option<Ipv4Addr>,

    /// Broadcast address to probe during discovery.
    #[arg(long, default_value_t = DEFAULT_BROADCAST)]
    pub broadcast: Ipv4Addr,

    /// TCP diagnostic port.
    #[arg(long, default_value_t = DIAG_PORT)]
    pub port: u16,

    /// Per-read timeout in ms (P2*).
    #[arg(long, default_value_t = P2_STAR_SERVER_MAX_DEFAULT_MS)]
    pub timeout: u64,

    /// TCP connect timeout in ms.
    #[arg(long, default_value_t = CONNECT_TIMEOUT_DEFAULT_MS)]
    pub connect_timeout: u64,

    /// Milliseconds to listen for discovery replies.
    #[arg(long, default_value_t = 2000)]
    pub discovery_wait: u64,

    /// Path to the ISTA-derived semantic database (read-only) for fault/DID/ECU text.
    #[arg(long, env = "KLARTEXT_SEMANTIC_DB", default_value = "data/klartext-semantic.db")]
    pub semantic_db: PathBuf,
}

impl ServerConfig {
    /// Build a client config targeting `ecu` (a logical address behind the ZGW).
    pub fn client_config(&self, ecu: u8) -> ClientConfig {
        ClientConfig {
            port: self.port,
            ecu,
            tester: TESTER_ADDRESS,
            connect_timeout: Duration::from_millis(self.connect_timeout),
            read_timeout: Duration::from_millis(self.timeout),
        }
    }

    /// The discovery listen window as a `Duration`.
    pub fn discovery_wait(&self) -> Duration {
        Duration::from_millis(self.discovery_wait)
    }
}
```

- [ ] **Step 5: Write `mcp/src/server.rs`** (struct + `disconnect` + handler; later tasks add the other tools and a `catalog()` helper):

```rust
//! The MCP server: read-only diagnostic tools over a held car session.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::DisconnectResult;

/// Shared server state: an optional held car connection (`None` = not connected).
type SessionState = Arc<Mutex<Option<()>>>;

/// The klartext MCP server. Cloneable handle over shared state.
#[derive(Clone)]
pub struct KlartextServer {
    #[allow(dead_code)]
    config: Arc<ServerConfig>,
    state: SessionState,
    tool_router: ToolRouter<KlartextServer>,
}

impl KlartextServer {
    /// Build the server. Does **not** connect to the car.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// The names of the tools this server advertises (for tests / introspection).
    pub fn advertised_tools(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

#[tool_router]
impl KlartextServer {
    #[tool(description = "Close the diagnostic session and release the car \
        connection. Safe to call when not connected.")]
    pub async fn disconnect(&self) -> Result<Json<DisconnectResult>, McpError> {
        let was_connected = self.state.lock().await.take().is_some();
        Ok(Json(DisconnectResult { was_connected }))
    }
}

#[tool_handler]
impl ServerHandler for KlartextServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Read-only BMW F-series diagnostics. Call connect first (discovers the \
                 gateway or uses a configured IP, reads the VIN). Then read_faults and \
                 read_data target an ECU by name (\"DME\"), hex address (\"0x12\"), or ISTA \
                 group name (\"d_0012\"); list_ecus enumerates targetable ECUs. This server \
                 cannot clear faults, actuate, or code — those are intentionally absent and \
                 live in the CLI with a human in the loop. Fault text and the full ECU map \
                 come from the ISTA SQLiteDB; reads still work (raw) without it."
                    .to_string(),
            )
    }
}
```

Note: `state` is `Arc<Mutex<Option<()>>>` for now; Task 4 replaces `()` with `session::Connection`.

- [ ] **Step 6: Write `mcp/src/lib.rs`:**

```rust
//! klartext-mcp — a read-only stdio MCP server over klartext-client/-semantic.
//!
//! Exposed as a library so integration tests can drive the tools in-process; the
//! `klartext-mcp` binary (`main.rs`) is a thin wrapper that serves over stdio.

pub mod config;
pub mod dto;
pub mod server;

pub use server::KlartextServer;
```

- [ ] **Step 7: Write `mcp/src/main.rs`:**

```rust
//! klartext-mcp binary: serve the read-only diagnostic tools over stdio.
//!
//! CRITICAL: stdout carries only the JSON-RPC stream. ALL logging goes to stderr.

use anyhow::Result;
use clap::Parser;
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // ALL logging to stderr — stdout is the JSON-RPC transport only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = ServerConfig::parse();
    tracing::info!(
        "klartext-mcp starting (read-only); semantic DB: {}",
        config.semantic_db.display()
    );

    let service = KlartextServer::new(config)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("failed to start MCP server: {e}"))?;
    service.waiting().await?;
    Ok(())
}
```

- [ ] **Step 8: Build, expect success.**

Run: `cargo build -p klartext-mcp`
Expected: compiles clean. (Resolves the entire rmcp/serde/schemars/clap wiring.)

- [ ] **Step 9: Write the scaffold integration test** in `mcp/tests/integration.rs`:

```rust
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;

/// Build a server config with defaults, overriding nothing (no car needed).
fn test_config() -> ServerConfig {
    ServerConfig::parse_from(["klartext-mcp"])
}

#[tokio::test]
async fn disconnect_without_connection_reports_not_connected() {
    let server = KlartextServer::new(test_config());
    let result = server.disconnect().await.unwrap();
    assert!(!result.0.was_connected);
}

#[test]
fn advertises_disconnect_tool() {
    let server = KlartextServer::new(test_config());
    let tools = server.advertised_tools();
    assert!(tools.contains(&"disconnect".to_string()), "tools: {tools:?}");
}
```

`ServerConfig::parse_from` needs `clap::Parser` in scope; add `use clap::Parser;` at the top of the test file.

- [ ] **Step 10: Run tests, expect pass.**

Run: `cargo test -p klartext-mcp`
Expected: both tests PASS.

- [ ] **Step 11: Lint + commit.**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings
git add mcp Cargo.toml Cargo.lock
git commit -m "feat(mcp): scaffold read-only stdio server + disconnect tool"
```

---

### Task 3: ECU registry + `list_ecus`

DB-driven, general ECU map with a built-in BMW-wide alias fallback, plus name/address resolution used by the read tools.

**Files:**
- Create: `mcp/src/ecu.rs`
- Modify: `mcp/src/dto.rs` (add `EcuInfo`, `ListEcusResult`)
- Modify: `mcp/src/server.rs` (add `catalog()` helper + `list_ecus` tool)
- Modify: `mcp/src/lib.rs` (add `pub mod ecu;`)
- Test: `mcp/src/ecu.rs` unit tests (no DB) + `mcp/tests/integration.rs` (with fixture DB)

**Interfaces:**
- Consumes: `klartext_semantic::{Catalog, EcuEntry}`, `klartext_hsfz::ZGW_ADDRESS`, `crate::dto::EcuInfo`.
- Produces: `crate::ecu::resolve(spec: &str, catalog: Option<&Catalog>) -> Result<u8, String>`; `crate::ecu::list(catalog: Option<&Catalog>) -> Vec<EcuInfo>`; `crate::dto::EcuInfo { address_hex: String, names: Vec<String>, group_name: Option<String>, source: String }`; `crate::dto::ListEcusResult { ecus: Vec<EcuInfo>, db_available: bool, note: String }`; `KlartextServer::catalog(&self) -> Option<Catalog>` and `pub async fn list_ecus(&self)`.

- [ ] **Step 1: Add the dto types.** Append to `mcp/src/dto.rs` (and add `use crate::ecu::EcuInfo;`? No — define `EcuInfo` here so `dto` has no dependency on `ecu`):

```rust
/// One targetable ECU for `list_ecus`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EcuInfo {
    /// Diagnostic address as hex, e.g. "0x12".
    pub address_hex: String,
    /// Known names: built-in aliases and/or the ISTA group name.
    pub names: Vec<String>,
    /// The ISTA group name (e.g. "d_0012"), when the DB provides it.
    pub group_name: Option<String>,
    /// Origin of this entry: "builtin", "db", or "builtin+db".
    pub source: String,
}

/// Result of `list_ecus`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListEcusResult {
    /// Targetable ECUs, ordered by address.
    pub ecus: Vec<EcuInfo>,
    /// Whether the semantic DB was available to enrich the list.
    pub db_available: bool,
    /// Human note about the source of the list.
    pub note: String,
}
```

- [ ] **Step 2: Write `mcp/src/ecu.rs` with failing-first unit tests.** Create the file:

```rust
//! ECU targeting: a built-in BMW-wide alias table plus the semantic DB's general
//! ECU map, with name/address resolution for the read tools.

use std::collections::BTreeMap;

use klartext_hsfz::ZGW_ADDRESS;
use klartext_semantic::Catalog;

use crate::dto::EcuInfo;

/// BMW-wide documented ECU aliases (report §2.4). Not car-specific; the full
/// per-model map comes from the semantic DB. [verify against capture].
const BUILTIN_ALIASES: &[(&str, u8)] = &[
    ("ZGW", ZGW_ADDRESS), // 0x10 — central gateway
    ("DME", 0x12),        // engine
    ("CAS", 0x40),        // car access system / body (FEM on later F-series)
];

/// Resolve an `ecu` parameter to a diagnostic address.
///
/// Order: raw hex address (`0x12`) → built-in alias (case-insensitive) → DB group
/// name (`d_0012`). Returns `Err(message)` naming `list_ecus` when unknown.
pub fn resolve(spec: &str, catalog: Option<&Catalog>) -> Result<u8, String> {
    let s = spec.trim();
    if let Some(addr) = parse_hex_address(s) {
        return Ok(addr);
    }
    if let Some((_, addr)) = BUILTIN_ALIASES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(s))
    {
        return Ok(*addr);
    }
    if let Some(catalog) = catalog {
        if let Ok(entries) = catalog.ecus() {
            if let Some(entry) = entries.iter().find(|e| e.group_name.eq_ignore_ascii_case(s)) {
                return Ok(entry.address);
            }
        }
    }
    Err(format!(
        "unknown ECU '{spec}'. Use a name (call list_ecus), an ISTA group name like \
         d_0012, or a raw hex address like 0x12."
    ))
}

/// List targetable ECUs: built-in aliases merged with the DB map (by address).
pub fn list(catalog: Option<&Catalog>) -> Vec<EcuInfo> {
    // address -> (names, group_name, from_builtin, from_db)
    let mut map: BTreeMap<u8, (Vec<String>, Option<String>, bool, bool)> = BTreeMap::new();
    for (name, addr) in BUILTIN_ALIASES {
        let entry = map.entry(*addr).or_default();
        entry.0.push((*name).to_string());
        entry.2 = true;
    }
    if let Some(catalog) = catalog {
        if let Ok(entries) = catalog.ecus() {
            for e in entries {
                let entry = map.entry(e.address).or_default();
                if entry.1.is_none() {
                    entry.1 = Some(e.group_name.clone());
                }
                if !entry.0.contains(&e.group_name) {
                    entry.0.push(e.group_name);
                }
                entry.3 = true;
            }
        }
    }
    map.into_iter()
        .map(|(addr, (names, group_name, builtin, db))| EcuInfo {
            address_hex: format!("0x{addr:02X}"),
            names,
            group_name,
            source: match (builtin, db) {
                (true, true) => "builtin+db",
                (true, false) => "builtin",
                _ => "db",
            }
            .to_string(),
        })
        .collect()
}

/// Parse a raw diagnostic address written as `0x12` / `0X12`. Bare decimals are
/// rejected (ambiguous with hex), so addresses are always explicit.
fn parse_hex_address(s: &str) -> Option<u8> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u8::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_raw_hex_address() {
        assert_eq!(resolve("0x12", None).unwrap(), 0x12);
        assert_eq!(resolve("0X40", None).unwrap(), 0x40);
    }

    #[test]
    fn resolve_builtin_alias_case_insensitive() {
        assert_eq!(resolve("DME", None).unwrap(), 0x12);
        assert_eq!(resolve("zgw", None).unwrap(), 0x10);
    }

    #[test]
    fn resolve_unknown_without_db_errors_with_hint() {
        let err = resolve("d_0012", None).unwrap_err();
        assert!(err.contains("list_ecus"), "{err}");
    }

    #[test]
    fn list_without_db_returns_builtins() {
        let ecus = list(None);
        let names: Vec<&str> = ecus.iter().flat_map(|e| e.names.iter().map(String::as_str)).collect();
        assert!(names.contains(&"ZGW"));
        assert!(names.contains(&"DME"));
        assert!(names.contains(&"CAS"));
        assert!(ecus.iter().all(|e| e.source == "builtin"));
    }
}
```

- [ ] **Step 3: Register the module.** In `mcp/src/lib.rs`, add `pub mod ecu;` (keep modules alphabetical: after `dto`).

- [ ] **Step 4: Run the ecu unit tests, expect pass.**

Run: `cargo test -p klartext-mcp --lib ecu`
Expected: all four `ecu::tests` PASS.

- [ ] **Step 5: Add `catalog()` + `list_ecus` to the server.** In `mcp/src/server.rs`, add imports at the top:

```rust
use klartext_semantic::Catalog;

use crate::dto::ListEcusResult;
use crate::ecu;
```

Add the helper inside the plain `impl KlartextServer` block (after `advertised_tools`):

```rust
    /// Open the semantic catalog read-only, or `None` when unavailable (degraded).
    fn catalog(&self) -> Option<Catalog> {
        match Catalog::open(&self.config.semantic_db) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("semantic DB unavailable ({e}); raw codes/names only");
                None
            }
        }
    }
```

Add the tool inside the `#[tool_router] impl KlartextServer` block:

```rust
    #[tool(description = "List the ECUs the read tools can target: built-in BMW \
        aliases plus, when the ISTA semantic DB is present, the full per-model ECU \
        address map. Does not require a connection.")]
    pub async fn list_ecus(&self) -> Result<Json<ListEcusResult>, McpError> {
        let catalog = self.catalog();
        let db_available = catalog.is_some();
        let ecus = ecu::list(catalog.as_ref());
        let note = if db_available {
            "Built-in aliases merged with the ISTA ECU map.".to_string()
        } else {
            "Built-in aliases only (no semantic DB). Target other ECUs by raw hex \
             address like 0x12."
                .to_string()
        };
        Ok(Json(ListEcusResult { ecus, db_available, note }))
    }
```

(`config` is now read in `catalog()`, so remove the `#[allow(dead_code)]` on the `config` field added in Task 2.)

- [ ] **Step 6: Add the DB-backed integration test.** Append to `mcp/tests/integration.rs` (add `use std::path::PathBuf;`, `use tempfile::TempDir;`, `use rusqlite::Connection;` at the top):

```rust
/// Build a synthetic semantic DB (no BMW data) matching the extract schema.
fn fixture_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
         CREATE TABLE ecu (address INT, variant TEXT, group_name TEXT);
         INSERT INTO dtc VALUES (64,'fem_20',14222346,NULL,'BEISPIEL Fehler A','EXAMPLE fault A: bus, no communication');
         INSERT INTO dtc VALUES (64,'fem_21',14222346,NULL,'BEISPIEL Fehler B','EXAMPLE fault B: bus communication fault');
         INSERT INTO ecu VALUES (16,'zgw_x','d_0010');
         INSERT INTO ecu VALUES (18,'dme_x','d_0012');
         INSERT INTO ecu VALUES (64,'fem_20','d_0040');",
    )
    .unwrap();
    (dir, path)
}

/// A server config pointed at a fixture DB (no gateway set).
fn config_with_db(path: &std::path::Path) -> ServerConfig {
    ServerConfig::parse_from(["klartext-mcp", "--semantic-db", path.to_str().unwrap()])
}

#[tokio::test]
async fn list_ecus_merges_builtin_and_db() {
    let (_dir, path) = fixture_db();
    let server = KlartextServer::new(config_with_db(&path));
    let result = server.list_ecus().await.unwrap();
    assert!(result.0.db_available);
    // 0x40 is both a builtin alias (CAS) and a DB group (d_0040).
    let cas = result.0.ecus.iter().find(|e| e.address_hex == "0x40").unwrap();
    assert_eq!(cas.source, "builtin+db");
    assert_eq!(cas.group_name.as_deref(), Some("d_0040"));
    assert!(cas.names.iter().any(|n| n == "CAS"));
    // 0x12 has both the DME alias and the d_0012 group.
    let dme = result.0.ecus.iter().find(|e| e.address_hex == "0x12").unwrap();
    assert_eq!(dme.source, "builtin+db");
}
```

- [ ] **Step 7: Run tests, expect pass.**

Run: `cargo test -p klartext-mcp`
Expected: scaffold tests + `list_ecus_merges_builtin_and_db` PASS. Verify `advertised_tools()` would now include `list_ecus` (covered fully in Task 7).

- [ ] **Step 8: Lint + commit.**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings
git add mcp
git commit -m "feat(mcp): DB-driven ECU map + list_ecus tool"
```

---

### Task 4: Session lifecycle + `connect` + mock-gateway harness

Adds the held connection, the establish/reconnect logic, the `connect` tool, and the reusable loopback mock gateway that Tasks 5–6 also use.

**Files:**
- Create: `mcp/src/session.rs`
- Modify: `mcp/src/dto.rs` (add `ConnectRequest`, `ConnectResult`)
- Modify: `mcp/src/server.rs` (use real `Connection` state; add `connect`)
- Modify: `mcp/src/lib.rs` (add `pub mod session;`)
- Test: `mcp/tests/integration.rs`

**Interfaces:**
- Consumes: `klartext_client::{DiagnosticClient, Gateway}`, `klartext_hsfz::ZGW_ADDRESS`, `klartext_semantic::did`, `crate::config::ServerConfig`.
- Produces: `crate::session::{Connection, SessionState, VinSource, establish, ensure_target}`; `crate::dto::{ConnectRequest, ConnectResult}`; `KlartextServer::connect`. `SessionState = Arc<Mutex<Option<Connection>>>`.

- [ ] **Step 1: Write `mcp/src/session.rs`:**

```rust
//! The ephemeral car connection held in server state, with establish + retarget.

use std::net::IpAddr;
use std::sync::Arc;

use klartext_client::{DiagnosticClient, Gateway};
use klartext_hsfz::ZGW_ADDRESS;
use tokio::sync::Mutex;

use crate::config::ServerConfig;

/// The DID for the VIN (ISO 14229 vehicleIdentificationNumber).
const DID_VIN: u16 = 0xF190;

/// Where a reported VIN came from.
#[derive(Debug, Clone, Copy)]
pub enum VinSource {
    /// Read authoritatively from the ZGW via DID 0xF190.
    DidF190,
    /// Best-effort from the discovery (0x11) announcement body.
    Discovery,
    /// No VIN could be obtained.
    Unknown,
}

impl VinSource {
    pub fn as_str(self) -> &'static str {
        match self {
            VinSource::DidF190 => "did_f190",
            VinSource::Discovery => "discovery",
            VinSource::Unknown => "unknown",
        }
    }
}

/// A live diagnostic connection. The client targets `target`; reads retarget it.
pub struct Connection {
    pub gateway_ip: IpAddr,
    pub vin: Option<String>,
    pub vin_source: VinSource,
    pub target: u8,
    pub client: DiagnosticClient,
}

/// Shared, mutable server connection state. `None` = not connected.
pub type SessionState = Arc<Mutex<Option<Connection>>>;

/// Establish a session to the gateway (ZGW) and read the VIN.
///
/// `gateway_ip` takes the direct path; `None` auto-discovers on the link. The
/// returned `Connection` targets the ZGW (0x10).
pub async fn establish(
    config: &ServerConfig,
    gateway_ip: Option<IpAddr>,
) -> Result<Connection, String> {
    let zgw_config = config.client_config(ZGW_ADDRESS);
    let (mut client, gateway): (DiagnosticClient, Option<Gateway>) = match gateway_ip {
        Some(ip) => {
            let client = DiagnosticClient::connect(ip, &zgw_config)
                .await
                .map_err(|e| format!("connect to {ip} failed: {e}"))?;
            (client, None)
        }
        None => {
            let (client, gateway) = DiagnosticClient::discover_and_connect(
                config.bind,
                config.broadcast,
                config.discovery_wait(),
                &zgw_config,
            )
            .await
            .map_err(|e| format!("gateway discovery/connect failed: {e}"))?;
            (client, Some(gateway))
        }
    };

    let ip = gateway_ip
        .or_else(|| gateway.as_ref().map(|g| g.ip))
        .expect("a gateway IP from the arg or discovery");

    // Authoritative VIN via DID F190; fall back to discovery's best-effort VIN.
    let did_vin = match client.read_did(DID_VIN).await {
        Ok((_, raw)) => klartext_semantic::did::decode(DID_VIN, &raw).text,
        Err(e) => {
            tracing::warn!("VIN read (DID F190) failed: {e}; using discovery VIN if any");
            None
        }
    };
    let discovery_vin = gateway.as_ref().and_then(|g| g.vin.clone());
    let (vin, vin_source) = match (did_vin, discovery_vin) {
        (Some(v), _) => (Some(v), VinSource::DidF190),
        (None, Some(v)) => (Some(v), VinSource::Discovery),
        (None, None) => (None, VinSource::Unknown),
    };

    Ok(Connection {
        gateway_ip: ip,
        vin,
        vin_source,
        target: ZGW_ADDRESS,
        client,
    })
}

/// Ensure the held connection targets `address`, reconnecting if it differs.
///
/// Reuses the warm session when the target matches; otherwise drops it (aborting
/// its keepalive on `Drop`) and opens a fresh client to the same gateway.
pub async fn ensure_target(
    conn: &mut Connection,
    config: &ServerConfig,
    address: u8,
) -> Result<(), String> {
    if conn.target == address {
        return Ok(());
    }
    let client = DiagnosticClient::connect(conn.gateway_ip, &config.client_config(address))
        .await
        .map_err(|e| format!("reconnect to ECU 0x{address:02X} failed: {e}"))?;
    conn.client = client;
    conn.target = address;
    Ok(())
}
```

- [ ] **Step 2: Add the `connect` dto types.** Append to `mcp/src/dto.rs` (add `use serde::Deserialize;` to the existing serde import line → `use serde::{Deserialize, Serialize};`):

```rust
/// Arguments for `connect`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConnectRequest {
    /// Optional gateway IP override, e.g. "169.254.39.12". Omit to use the
    /// configured gateway or auto-discover on the link.
    #[serde(default)]
    pub gateway_ip: Option<String>,
}

/// Result of `connect`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ConnectResult {
    pub connected: bool,
    pub gateway_ip: String,
    pub vin: Option<String>,
    /// "did_f190" | "discovery" | "unknown".
    pub vin_source: String,
    pub target_ecu: String,
    pub note: String,
}
```

- [ ] **Step 3: Switch server state to `Connection` + add `connect`.** In `mcp/src/server.rs`:

Replace the `SessionState` type alias line with an import from `session`:

```rust
use crate::session::{self, SessionState};
```

and delete the local `type SessionState = Arc<Mutex<Option<()>>>;`. Update the `connect`/`disconnect` dto import line to:

```rust
use crate::dto::{ConnectRequest, ConnectResult, DisconnectResult, ListEcusResult};
```

(`Arc`/`Mutex` imports stay — `new()` still builds `Arc::new(Mutex::new(None))`, now `Option<Connection>`.)

Add the tool inside the `#[tool_router] impl`:

```rust
    #[tool(description = "Connect to the car's gateway over HSFZ and start a \
        read-only diagnostic session. Call this first. Discovers the gateway on the \
        link (or uses the provided/configured gateway IP), reads the VIN, and holds \
        the session open with a background keepalive. Returns the gateway IP and VIN.")]
    pub async fn connect(
        &self,
        Parameters(req): Parameters<ConnectRequest>,
    ) -> Result<Json<ConnectResult>, McpError> {
        let gateway_ip = match req.gateway_ip.as_deref() {
            Some(s) => Some(
                s.parse()
                    .map_err(|_| McpError::invalid_params(format!("invalid gateway_ip '{s}'"), None))?,
            ),
            None => self.config.gateway_ip,
        };
        let conn = session::establish(&self.config, gateway_ip)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        let result = ConnectResult {
            connected: true,
            gateway_ip: conn.gateway_ip.to_string(),
            vin: conn.vin.clone(),
            vin_source: conn.vin_source.as_str().to_string(),
            target_ecu: format!("ZGW (0x{:02X})", conn.target),
            note: "Read-only session held. Use read_faults/read_data; call disconnect when done."
                .to_string(),
        };
        *self.state.lock().await = Some(conn);
        Ok(Json(result))
    }
```

- [ ] **Step 4: Register the module.** In `mcp/src/lib.rs`, add `pub mod session;` (after `pub mod server;`).

- [ ] **Step 5: Build, expect success.**

Run: `cargo build -p klartext-mcp`
Expected: compiles. (`disconnect`'s `take()` now returns `Option<Connection>`; still fine.)

- [ ] **Step 6: Add the mock gateway + connect test.** Prepend the mock helper to `mcp/tests/integration.rs` (add `use std::net::Ipv4Addr;`, `use std::time::Duration;`, `use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};`, `use tokio::net::TcpListener;`):

```rust
/// A loopback mock gateway: answers VIN + DTC reads, ignores keepalives, and
/// accepts reconnections (each `ensure_target` opens a fresh connection).
async fn spawn_mock_gateway() -> std::net::SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    match frame.payload.as_slice() {
                        [0x3E, 0x80] => {} // keepalive — no reply
                        [0x22, 0xF1, 0x90] => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(b"WBA3B5C50EK123456");
                            let _ = write_frame(&mut stream, &HsfzFrame::diagnostic(0x10, 0xF4, uds)).await;
                        }
                        [0x19, 0x02, _mask] => {
                            // one DTC: code D9 04 0A (== 14222346), status 0x08 (confirmed).
                            let uds = vec![0x59, 0x02, 0xFF, 0xD9, 0x04, 0x0A, 0x08];
                            let _ = write_frame(&mut stream, &HsfzFrame::diagnostic(0x10, 0xF4, uds)).await;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    addr
}

/// A server config pointed at the mock gateway + a fixture DB.
fn config_for_mock(addr: std::net::SocketAddr, db: &std::path::Path) -> ServerConfig {
    ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
    ])
}

#[tokio::test]
async fn connect_returns_vin_from_the_gateway() {
    let addr = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));

    let result = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert!(result.0.connected);
    assert_eq!(result.0.vin.as_deref(), Some("WBA3B5C50EK123456"));
    assert_eq!(result.0.vin_source, "did_f190");
}
```

Add imports the test now needs: `use klartext_mcp::dto::ConnectRequest;` and `use rmcp::handler::server::wrapper::Parameters;`.

- [ ] **Step 7: Run tests, expect pass.**

Run: `cargo test -p klartext-mcp`
Expected: all prior tests + `connect_returns_vin_from_the_gateway` PASS.

- [ ] **Step 8: Lint + commit.**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings
git add mcp
git commit -m "feat(mcp): session lifecycle + connect tool + mock-gateway harness"
```

---

### Task 5: `read_faults`

Reads + decodes stored DTCs from a named ECU: ISO status flags + DB fault text, raw fields always present, reconnecting to the target when needed.

**Files:**
- Modify: `mcp/src/dto.rs` (add `FaultDescription`, `FaultInfo`, `ReadFaultsRequest`, `ReadFaultsResult`)
- Modify: `mcp/src/server.rs` (add `read_faults` + `not_connected` helper)
- Test: `mcp/tests/integration.rs`

**Interfaces:**
- Consumes: `klartext_uds::ALL_DTC_STATUS_MASK`, `klartext_semantic::dtc::status_flags`, `klartext_client::DiagnosticClient::read_dtcs`, `crate::session::ensure_target`, `crate::ecu::resolve`.
- Produces: `crate::dto::{FaultDescription, FaultInfo, ReadFaultsRequest, ReadFaultsResult}`; `KlartextServer::read_faults`; `fn not_connected() -> McpError`.

- [ ] **Step 1: Add the dto types.** Append to `mcp/src/dto.rs`:

```rust
/// Target ECU for `read_faults`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFaultsRequest {
    /// ECU: a name (e.g. "DME"), a hex address ("0x12"), or an ISTA group name
    /// ("d_0012"). Call list_ecus to discover targetable ECUs.
    pub ecu: String,
}

/// One per-variant human description for a fault.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultDescription {
    pub variant: String,
    pub saecode: Option<String>,
    pub text: Option<String>,
}

/// One decoded fault: raw code/status plus ISO flags and descriptions.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultInfo {
    pub code_hex: String,
    pub status_hex: String,
    pub status_flags: Vec<String>,
    pub descriptions: Vec<FaultDescription>,
}

/// Result of `read_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadFaultsResult {
    pub ecu: String,
    pub address: String,
    pub count: usize,
    pub faults: Vec<FaultInfo>,
    pub db_available: bool,
}
```

- [ ] **Step 2: Add the tool.** In `mcp/src/server.rs`, extend imports:

```rust
use klartext_semantic::dtc::status_flags;
use klartext_uds::ALL_DTC_STATUS_MASK;
```

and the dto import line to include the new types:

```rust
use crate::dto::{
    ConnectRequest, ConnectResult, DisconnectResult, FaultDescription, FaultInfo, ListEcusResult,
    ReadFaultsRequest, ReadFaultsResult,
};
```

Add the tool inside the `#[tool_router] impl`:

```rust
    #[tool(description = "Read and decode stored fault codes (DTCs) from one ECU. \
        Requires a prior connect. `ecu` is a name (\"DME\"), a hex address (\"0x12\"), \
        or an ISTA group name (\"d_0012\") — see list_ecus. Returns each fault's raw \
        code, decoded ISO status flags, and human description text (when the semantic \
        DB is available).")]
    pub async fn read_faults(
        &self,
        Parameters(req): Parameters<ReadFaultsRequest>,
    ) -> Result<Json<ReadFaultsResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;

        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        let dtcs = conn
            .client
            .read_dtcs(ALL_DTC_STATUS_MASK)
            .await
            .map_err(|e| McpError::internal_error(format!("reading DTCs: {e}"), None))?;

        let faults: Vec<FaultInfo> = dtcs
            .iter()
            .map(|d| {
                let descriptions = catalog
                    .as_ref()
                    .and_then(|c| c.describe_dtc(address, d.code).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|desc| FaultDescription {
                        variant: desc.ecu_variant,
                        saecode: desc.saecode,
                        text: desc.title_en.or(desc.title_de),
                    })
                    .collect();
                FaultInfo {
                    code_hex: format!("{:02X}{:02X}{:02X}", d.code[0], d.code[1], d.code[2]),
                    status_hex: format!("{:02X}", d.status),
                    status_flags: status_flags(d.status).into_iter().map(String::from).collect(),
                    descriptions,
                }
            })
            .collect();

        Ok(Json(ReadFaultsResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            count: faults.len(),
            faults,
            db_available: catalog.is_some(),
        }))
    }
```

Add the shared helper at the bottom of `server.rs` (module scope, outside the impls):

```rust
/// The clear, non-panicking error returned by read tools with no live session.
fn not_connected() -> McpError {
    McpError::invalid_request("not connected — call connect first", None)
}
```

- [ ] **Step 3: Add tests.** Append to `mcp/tests/integration.rs` (add `use klartext_mcp::dto::ReadFaultsRequest;`):

```rust
#[tokio::test]
async fn read_faults_decodes_flags_and_descriptions() {
    let addr = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // 0x40 is in the fixture's dtc + ecu tables (group d_0040).
    let result = server
        .read_faults(Parameters(ReadFaultsRequest { ecu: "0x40".to_string() }))
        .await
        .unwrap();
    assert_eq!(result.0.address, "0x40");
    assert_eq!(result.0.count, 1);
    let fault = &result.0.faults[0];
    assert_eq!(fault.code_hex, "D9040A");
    assert_eq!(fault.status_hex, "08");
    assert_eq!(fault.status_flags, vec!["confirmedDTC".to_string()]);
    assert!(result.0.db_available);
    // Two variants share the code at address 0x40.
    assert_eq!(fault.descriptions.len(), 2);
    assert!(fault.descriptions.iter().any(|d| d
        .text
        .as_deref()
        .is_some_and(|t| t.contains("EXAMPLE fault A"))));
}

#[tokio::test]
async fn read_faults_without_connect_errors_clearly() {
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_with_db(&db));
    let err = server
        .read_faults(Parameters(ReadFaultsRequest { ecu: "0x40".to_string() }))
        .await
        .unwrap_err();
    assert!(err.message.contains("not connected"), "{}", err.message);
}
```

- [ ] **Step 4: Run tests, expect pass.**

Run: `cargo test -p klartext-mcp`
Expected: all PASS, including the reconnect (connect targets ZGW 0x10, then `read_faults` retargets to 0x40) and the not-connected error.

- [ ] **Step 5: Lint + commit.**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings
git add mcp
git commit -m "feat(mcp): read_faults tool (decoded DTCs + status flags)"
```

---

### Task 6: `read_data`

Reads + decodes one DID from a named ECU: ISO-standard name + text rendering, raw always included.

**Files:**
- Modify: `mcp/src/dto.rs` (add `ReadDataRequest`, `ReadDataResult`)
- Modify: `mcp/src/server.rs` (add `read_data` + `parse_hex_u16` helper)
- Test: `mcp/tests/integration.rs`

**Interfaces:**
- Consumes: `klartext_semantic::did`, `klartext_client::DiagnosticClient::read_did`, `crate::session::ensure_target`, `crate::ecu::resolve`, `not_connected`.
- Produces: `crate::dto::{ReadDataRequest, ReadDataResult}`; `KlartextServer::read_data`; `fn parse_hex_u16(&str) -> Result<u16, String>`.

- [ ] **Step 1: Add the dto types.** Append to `mcp/src/dto.rs`:

```rust
/// Target ECU + DID for `read_data`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadDataRequest {
    /// ECU: a name ("DME"), hex address ("0x12"), or ISTA group name ("d_0012").
    pub ecu: String,
    /// Data identifier to read, hex (e.g. "F190" for the VIN, with or without 0x).
    pub did: String,
}

/// Result of `read_data`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadDataResult {
    pub ecu: String,
    pub address: String,
    pub did_hex: String,
    pub name: Option<String>,
    pub value_text: Option<String>,
    pub raw_hex: String,
    pub note: String,
}
```

- [ ] **Step 2: Add the tool.** In `mcp/src/server.rs`, extend the semantic import to bring in `did`:

```rust
use klartext_semantic::dtc::status_flags;
use klartext_semantic::{Catalog, did};
```

(merge with the existing `use klartext_semantic::Catalog;` line — final form shown above) and add `ReadDataRequest, ReadDataResult` to the `crate::dto::{...}` import list.

Add the tool inside the `#[tool_router] impl`:

```rust
    #[tool(description = "Read and decode one data identifier (DID) from an ECU. \
        Requires a prior connect. `ecu` as in read_faults; `did` is hex (e.g. \
        \"F190\" for the VIN). ISO-standard identification DIDs (0xF1xx) are named; \
        other DIDs return the raw value (BMW-specific scaling is out of scope).")]
    pub async fn read_data(
        &self,
        Parameters(req): Parameters<ReadDataRequest>,
    ) -> Result<Json<ReadDataResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let did = parse_hex_u16(&req.did).map_err(|e| McpError::invalid_params(e, None))?;

        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        let (got_did, raw) = conn
            .client
            .read_did(did)
            .await
            .map_err(|e| McpError::internal_error(format!("reading DID 0x{did:04X}: {e}"), None))?;

        let decoded = did::decode(got_did, &raw);
        let raw_hex = raw.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
        let note = if decoded.name.is_none() {
            "BMW-specific DID — name/scaling not in the SQLiteDB; raw value only.".to_string()
        } else {
            String::new()
        };
        Ok(Json(ReadDataResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            did_hex: format!("{got_did:04X}"),
            name: decoded.name.map(String::from),
            value_text: decoded.text,
            raw_hex,
            note,
        }))
    }
```

Add the helper at module scope (next to `not_connected`):

```rust
/// Parse a hex u16 DID with or without a `0x` prefix.
fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u16::from_str_radix(t, 16).map_err(|e| format!("invalid DID hex '{s}': {e}"))
}
```

- [ ] **Step 3: Add tests.** Append to `mcp/tests/integration.rs` (add `use klartext_mcp::dto::ReadDataRequest;`):

```rust
#[tokio::test]
async fn read_data_decodes_vin() {
    let addr = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "ZGW".to_string(),
            did: "F190".to_string(),
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "F190");
    assert_eq!(result.0.name.as_deref(), Some("VIN"));
    assert_eq!(result.0.value_text.as_deref(), Some("WBA3B5C50EK123456"));
}

#[tokio::test]
async fn read_data_rejects_bad_did_hex() {
    let addr = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    let err = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "ZGW".to_string(),
            did: "ZZZZ".to_string(),
        }))
        .await
        .unwrap_err();
    assert!(err.message.contains("invalid DID hex"), "{}", err.message);
}
```

- [ ] **Step 4: Run tests, expect pass.**

Run: `cargo test -p klartext-mcp`
Expected: all PASS.

- [ ] **Step 5: Lint + commit.**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings
git add mcp
git commit -m "feat(mcp): read_data tool (decoded DID values)"
```

---

### Task 7: Tool-surface safety assertion + final verification + docs

Locks the read-only contract with a test, confirms no stdout pollution, and updates CLAUDE.md.

**Files:**
- Test: `mcp/tests/integration.rs`
- Modify: `CLAUDE.md`

**Interfaces:**
- Consumes: `KlartextServer::advertised_tools`.

- [ ] **Step 1: Assert the exact read-only tool set.** Append to `mcp/tests/integration.rs`:

```rust
#[test]
fn advertises_exactly_the_five_read_only_tools() {
    let server = KlartextServer::new(test_config());
    let mut tools = server.advertised_tools();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "connect".to_string(),
            "disconnect".to_string(),
            "list_ecus".to_string(),
            "read_data".to_string(),
            "read_faults".to_string(),
        ]
    );
    // No mutating tool may ever appear.
    for forbidden in ["clear", "clear_faults", "clear_dtcs", "write", "code", "coding", "actuate", "io_control"] {
        assert!(
            !tools.iter().any(|t| t.contains(forbidden)),
            "forbidden tool present: {forbidden}"
        );
    }
}
```

- [ ] **Step 2: Run the full suite, expect pass.**

Run: `cargo test -p klartext-mcp`
Expected: all PASS (8 integration tests + ecu unit tests).

- [ ] **Step 3: Verify no stdout writes anywhere in the crate.**

Run: `rg -n 'println!|print!|eprint!|eprintln!|dbg!' mcp/src mcp/tests`
Expected: **no matches.** (All output is `tracing` → stderr, or tool return values.) If anything matches, replace it with `tracing::{info,warn,error}!` or remove it.

- [ ] **Step 4: Full workspace gate.**

```bash
cargo build
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```
Expected: all clean / green across the whole workspace (M1–M3 untouched behavior + the new crate).

- [ ] **Step 5: Confirm no BMW data is committed.**

Run: `git status --short && rg -n 'WBA[A-HJ-NPR-Z0-9]{14}' -g '!docs/**' -g '!**/tests/**' mcp crates || echo 'no embedded VINs in source'`
Expected: only source files staged; the only VIN literals are the synthetic `WBA3B5C50EK123456` in test code. No `.db`, `.pcap`, or `data/` content staged.

- [ ] **Step 6: Update CLAUDE.md.** Under `## Stack`, the crate-layout sentence currently ends with the binary convention. Append `mcp/` to the concrete layout note so the M4 crate is recorded. Find the line listing today's crates and add to it (exact insertion — append this sentence to the layout paragraph):

```
Today also: binary `mcp/` (`klartext-mcp`, the read-only stdio MCP server) over `klartext-client` + `klartext-semantic`; `klartext-semantic` exposes the general ECU map via `Catalog::ecus()`.
```

- [ ] **Step 7: Final commit.**

```bash
git add mcp/tests/integration.rs CLAUDE.md
git commit -m "test(mcp): assert exact read-only tool surface; docs: record mcp crate"
```

---

## Self-Review

**1. Spec coverage** (each spec section → task):
- Crate `mcp/` lib+bin, workspace member, deps → Task 2. ✓
- Session model (reconnect-per-target, ephemeral, keepalive, no startup connect) → Task 4 (`establish`/`ensure_target`), `connect`/`disconnect` Tasks 4/2. ✓
- ECU model (builtin aliases + `Catalog::ecus()` + resolve order raw→alias→group; `list_ecus` merge) → Task 1 + Task 3. ✓
- Five tools with AI-facing descriptions + structured `Json<T>` → Tasks 2–6; descriptions written in each `#[tool]`. ✓
- Not-connected clear error → Task 5 (`not_connected`), tested Tasks 5/6. ✓
- Semantic DB read-only per call, degrade gracefully → `catalog()` Task 3; degradation exercised by `db_available` + None-catalog ecu tests. ✓
- stderr-only logging, no stdout → Task 2 (`main.rs`) + Task 7 grep. ✓
- Config env/args → Task 2 (`config.rs`). ✓
- Tests vs mock gateway; list-tools exactness; no mutating tool → Tasks 4–7. ✓
- Verify checklist (build/clippy/fmt, surface, mock flows, no stdout, no startup connect, no DB committed) → Task 7. ✓

**2. Placeholder scan:** No TBD/TODO; every code step shows complete code; commands have expected output. ✓

**3. Type consistency:** `ServerConfig`/`client_config`/`discovery_wait` (Task 2) used consistently in Tasks 4–6. `SessionState`/`Connection`/`establish`/`ensure_target` (Task 4) match server usage. `Catalog::ecus()`/`EcuEntry` (Task 1) match `ecu.rs` (Task 3). `EcuInfo` defined in `dto.rs`, consumed in `ecu.rs` — single definition. dto names (`ConnectResult`, `ReadFaultsResult`, `FaultInfo`, `ReadDataResult`, `ListEcusResult`, `DisconnectResult`) match their tool return types. `not_connected()`/`parse_hex_u16` defined once. Tool method names (`connect`, `read_faults`, `read_data`, `list_ecus`, `disconnect`) match the Task 7 surface assertion. ✓

**Known build-order note for the implementer:** `mcp/src/server.rs` accumulates imports across Tasks 2→6. Each task's step says exactly which import line to extend; after Task 6 the semantic import is `use klartext_semantic::{Catalog, did}; use klartext_semantic::dtc::status_flags;` and the dto import lists all six result types + three request types. If an "unused import" warning appears mid-task, it resolves when the consuming tool is added in the same task.
