//! klartext CLI — auto-discover a BMW F-series gateway over HSFZ, then scan the
//! whole car, read fault codes and data identifiers from a chosen ECU, clear faults
//! (one ECU or all), or run a service function — every write behind explicit
//! confirmation. Faults and identifiers are decoded to human text via the
//! ISTA-derived semantic database (`klartext-semantic`).
//!
//! The default connect path is auto-discovery: the tool broadcasts an HSFZ
//! identification request, finds the gateway, and connects. `--gateway-ip` skips
//! discovery and connects directly (the M1 path). Reads are autonomous-safe;
//! `clear-faults` and `service run` are state changes and refuse to run without
//! `--confirm`. `service run` additionally refuses high-risk actuation/calibration
//! outright — those are human-driven only (the blast-radius rule, M7).

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use klartext_client::{
    ClientConfig, DEFAULT_BROADCAST, DiagnosticClient, EcuFaults, FaultDetailRaw, Gateway,
    ScanOptions,
};
use klartext_hsfz::{
    CONNECT_TIMEOUT_DEFAULT_MS, CONTROL_PORT, DIAG_PORT, TESTER_ADDRESS, ZGW_ADDRESS, discover,
    link_local_bind_ip,
};
use klartext_semantic::{
    Catalog, Category, FreezeFrameDefs, Measurements, Risk, ServiceFunction, ServiceFunctions,
    build_cbs_read_request, build_cbs_reset_request, build_read_request, did, dtc::status_flags,
};
use klartext_uds::{Dtc, P2_STAR_SERVER_MAX_DEFAULT_MS};

#[derive(Parser)]
#[command(
    version,
    about = "Auto-discover a BMW F-series gateway over HSFZ and read/clear diagnostics."
)]
struct Cli {
    /// Connect directly to this gateway IP, skipping discovery (M1 fallback).
    #[arg(long, env = "KLARTEXT_GATEWAY", global = true)]
    gateway_ip: Option<IpAddr>,

    /// Bind discovery to this link-local source IP (default: auto-detect).
    #[arg(long, env = "KLARTEXT_BIND", global = true)]
    bind: Option<Ipv4Addr>,

    /// Broadcast address to probe during discovery.
    #[arg(long, default_value_t = DEFAULT_BROADCAST, global = true)]
    broadcast: Ipv4Addr,

    /// Target ECU logical address (hex, e.g. 10 = gateway, 12 = engine).
    #[arg(long, value_parser = parse_hex_u8, default_value = "0x10", env = "KLARTEXT_TARGET", global = true)]
    target: u8,

    /// TCP diagnostic port.
    #[arg(long, default_value_t = DIAG_PORT, global = true)]
    port: u16,

    /// Per-read timeout in ms (P2*).
    #[arg(long, default_value_t = P2_STAR_SERVER_MAX_DEFAULT_MS, global = true)]
    timeout: u64,

    /// TCP connect timeout in ms.
    #[arg(long, default_value_t = CONNECT_TIMEOUT_DEFAULT_MS, global = true)]
    connect_timeout: u64,

    /// Milliseconds to listen for discovery replies.
    #[arg(long, default_value_t = 2000, global = true)]
    discovery_wait: u64,

    /// Per-ECU presence-probe timeout in ms for `scan` and whole-car operations.
    #[arg(long, default_value_t = 300, global = true)]
    probe_timeout: u64,

    /// How many ECUs to probe/read at once (1 = strictly sequential).
    #[arg(long, default_value_t = 8, global = true)]
    scan_concurrency: usize,

    /// Path to the ISTA-derived semantic database for fault/DID decoding.
    #[arg(
        long,
        env = "KLARTEXT_SEMANTIC_DB",
        default_value = "data/klartext-semantic.db",
        global = true
    )]
    semantic_db: PathBuf,

    /// Path to the target ECU's SGBD `.prg`, enabling proprietary measurement
    /// scaling (`SG_FUNKTIONEN`). BYO-data; omit to keep BMW-specific DIDs raw.
    #[arg(long, env = "KLARTEXT_SGBD", global = true)]
    sgbd: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Broadcast an HSFZ identification request and print the gateways found.
    Discover,
    /// Scan the whole car: find fitted ECUs and summarize each one's faults.
    Scan {
        /// Only list fitted ECUs; skip reading their faults.
        #[arg(long)]
        ecus_only: bool,
    },
    /// Read fault codes (DTCs) from the target ECU.
    ReadFaults {
        /// DTC status mask (hex): which status bits to match. FF = all stored.
        #[arg(long, value_parser = parse_hex_u8, default_value = "0xFF")]
        mask: u8,
        /// Also print the raw 3-byte code / status byte.
        #[arg(long)]
        raw: bool,
        /// Also show "not tested this cycle" catalog entries (default: hidden, counted).
        #[arg(long)]
        all: bool,
    },
    /// Read one fault's freeze-frame / snapshot metadata (UDS 19 04 / 06 / 09).
    ///
    /// The environmental conditions the ECU latched when the fault occurred
    /// (mileage, timestamp, RPM, temperatures, ECU state) plus occurrence/healing
    /// counters and severity. Pass `--sgbd <ecu>.prg` to decode the fields; without
    /// it the raw region is shown. The framing is derived, pending an on-car capture.
    FaultDetail {
        /// The 3-byte DTC as hex, e.g. 240000 (a code from `read-faults`).
        #[arg(value_parser = parse_dtc_arg)]
        code: [u8; 3],
    },
    /// Read a data identifier (DID) value from the target ECU.
    ReadDid {
        /// The DID to read (hex). Defaults to F190 = VIN.
        #[arg(value_parser = parse_hex_u16, default_value = "F190")]
        did: u16,
        /// Also print the raw value bytes.
        #[arg(long)]
        raw: bool,
    },
    /// Clear all fault codes on the target ECU (state change — needs --confirm).
    ClearFaults {
        /// Confirm this state-changing write; without it the command refuses.
        #[arg(long)]
        confirm: bool,
        /// Clear across ALL fitted ECUs, not just --target (still needs --confirm).
        #[arg(long)]
        all_ecus: bool,
    },
    /// Send a TesterPresent to the target ECU (a connectivity check).
    TesterPresent,
    /// List the target ECU's service functions (resets, actuations, calibrations).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

/// Service-function subcommands: offline discovery (`list`) and the gated,
/// low-risk-only execute path (`run`).
#[derive(Subcommand)]
enum ServiceAction {
    /// List the service functions discoverable in the target ECU's SGBD `.prg`.
    ///
    /// Offline — needs `--sgbd <ecu>.prg`, not a car. Functions are grouped by
    /// category and tagged with blast-radius risk (low = resets, high = actuation).
    List,
    /// Run a low-risk service function by label (e.g. `Oel` = oil reset). A write —
    /// needs `--confirm`. High-risk actuation/calibration is refused (human-only).
    Run {
        /// The function's label from `service list` (e.g. `Oel`, `Br_v`).
        label: String,
        /// Confirm this state-changing write; without it the command refuses.
        #[arg(long)]
        confirm: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Discover => {
            run_discover(&cli).await?;
        }
        Command::Scan { ecus_only } => run_scan(&cli, *ecus_only).await?,
        Command::ReadFaults { mask, raw, all } => {
            let (client, _gateway) = connect(&cli).await?;
            let dtcs = client
                .read_dtcs(cli.target, *mask)
                .await
                .context("reading DTCs")?;
            let catalog = open_catalog(&cli.semantic_db);
            print_faults(&dtcs, cli.target, catalog.as_ref(), *raw, *all);
            print_verify_list();
        }
        Command::FaultDetail { code } => {
            let (client, _gateway) = connect(&cli).await?;
            let detail = client
                .read_fault_detail(cli.target, *code)
                .await
                .context("reading fault detail")?;
            let catalog = open_catalog(&cli.semantic_db);
            let defs = open_freeze_frame_defs(cli.sgbd.as_deref());
            print_fault_detail(cli.target, *code, &detail, defs.as_ref(), catalog.as_ref());
            print_verify_list();
        }
        Command::ReadDid { did, raw } => {
            let (client, _gateway) = connect(&cli).await?;
            let measurements = open_measurements(cli.sgbd.as_deref());
            // M6 Part B: a dynamic SG_FUNKTIONEN measurement (SERVICE "22;2C") reads
            // via the 0x2C define + 0x22 read sequence; a static DID is a plain 0x22
            // read. Either way the requested id is shown (not the dynamic 0xF303).
            let (got_did, value) = match measurements.as_ref().and_then(|m| m.get(*did)) {
                Some(measurement) if measurement.is_dynamic() => {
                    let requests = build_read_request(measurement);
                    let value = client
                        .read_dynamic_measurement(cli.target, &requests)
                        .await
                        .context("reading dynamic measurement")?;
                    (*did, value)
                }
                _ => client
                    .read_did(cli.target, *did)
                    .await
                    .context("reading DID")?,
            };
            print_did_value(got_did, &value, cli.target, *raw, measurements.as_ref());
            print_verify_list();
        }
        Command::ClearFaults { confirm, all_ecus } => {
            run_clear_faults(&cli, *confirm, *all_ecus).await?;
        }
        Command::TesterPresent => {
            let (client, _gateway) = connect(&cli).await?;
            client
                .tester_present(cli.target)
                .await
                .context("sending TesterPresent")?;
            println!("✔ TesterPresent acknowledged by ECU 0x{:02X}.", cli.target);
        }
        Command::Service { action } => match action {
            // Offline discovery: read the SGBD, list functions; no car connection.
            ServiceAction::List => run_service_list(&cli)?,
            ServiceAction::Run { label, confirm } => run_service_run(&cli, label, *confirm).await?,
        },
    }
    Ok(())
}

/// The `service list` subcommand: read the SGBD control catalog and print it.
///
/// Offline — it never connects to the car. Requires `--sgbd <ecu>.prg`, since the
/// control catalog is built from the ECU's EDIABAS SGBD (BYO-data).
fn run_service_list(cli: &Cli) -> Result<()> {
    let Some(sgbd) = cli.sgbd.as_deref() else {
        bail!("`service list` needs the target ECU's SGBD: pass --sgbd <ecu>.prg");
    };
    let functions = ServiceFunctions::from_sgbd(sgbd)
        .with_context(|| format!("reading SGBD {}", sgbd.display()))?;
    print_service_functions(&functions, sgbd);
    Ok(())
}

/// The blast-radius decision `service run` makes for a function, before any car I/O.
///
/// Kept as a pure value (see [`classify_run`]) so the safety gate is unit-testable
/// without a car or BYO SGBD data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunDecision {
    /// HIGH-risk actuation/calibration — refused outright (human-driven only).
    RefuseHighRisk,
    /// Low-risk, but no frame is derivable offline — discovery-only, refused.
    RefuseNotDerivable,
    /// Runnable, but `--confirm` was not given.
    NeedsConfirm,
    /// Run the CBS reset path (write + read-back).
    RunCbs,
    /// Run a generic one-shot derived reset.
    RunGeneric,
}

/// Decide what `service run` does with `function` given `confirm` — pure, no I/O.
///
/// Risk is the outer gate ([`Risk::High`] is always refused, whatever its derivation),
/// derivation is the inner gate (a low-risk function with no derived frame is
/// discovery-only), and `--confirm` gates every runnable frame.
fn classify_run(function: &ServiceFunction, confirm: bool) -> RunDecision {
    if function.risk() == Risk::High {
        return RunDecision::RefuseHighRisk;
    }
    if !function.is_derived() {
        return RunDecision::RefuseNotDerivable;
    }
    if !confirm {
        return RunDecision::NeedsConfirm;
    }
    if function.category == Category::CbsReset {
        RunDecision::RunCbs
    } else {
        RunDecision::RunGeneric
    }
}

/// The `service run <label>` subcommand: execute a low-risk, derived service function.
///
/// Blast-radius gated via [`classify_run`]: high-risk actuation/calibration is refused
/// outright (human-driven only), a low-risk function with no offline-derived frame is
/// refused as discovery-only, and a runnable low-risk reset still requires `--confirm`.
async fn run_service_run(cli: &Cli, label: &str, confirm: bool) -> Result<()> {
    let Some(sgbd) = cli.sgbd.as_deref() else {
        bail!("`service run` needs the target ECU's SGBD: pass --sgbd <ecu>.prg");
    };
    let functions = ServiceFunctions::from_sgbd(sgbd)
        .with_context(|| format!("reading SGBD {}", sgbd.display()))?;
    let Some(function) = functions.by_label(label) else {
        bail!(
            "no service function labelled `{label}` in {}. Run `service list` to see them.",
            sgbd.display()
        );
    };

    match classify_run(function, confirm) {
        // The W2 line: anything that moves a component or alters calibration is
        // human-driven only and never executes from here.
        RunDecision::RefuseHighRisk => bail!(
            "`{}` ({}) is a HIGH-risk {} — it moves a physical component or alters calibration. \
             It is human-driven only (never autonomous, never over MCP) and is not available in \
             this CLI. Use ISTA / a workshop tool with the required preconditions.",
            function.label,
            function.name,
            category_title(function.category)
        ),
        RunDecision::RefuseNotDerivable => bail!(
            "`{}` ({}) is a low-risk {}, but its exact UDS frame is not derivable offline, so it \
             is discovery-only (not executable) in this build:\n  {}\n\
             See docs/service-functions-findings.md.",
            function.label,
            function.name,
            category_title(function.category),
            function
                .derivation
                .reason()
                .unwrap_or("frame not derivable offline"),
        ),
        // CBS resets word the confirm prompt around the dashboard indicator; other
        // derived resets around watching the ECU's behaviour (no read-back).
        RunDecision::NeedsConfirm if function.category == Category::CbsReset => bail!(
            "Resetting the {} service counter on ECU 0x{:02X} is a state change (UDS 0x2E write). \
             Re-run with --confirm to proceed.\n\
             Note: the frame is DERIVED from ISTA data and unconfirmed on a car — verify the \
             dashboard service indicator afterward.",
            function.name,
            cli.target
        ),
        RunDecision::NeedsConfirm => bail!(
            "Running {} on ECU 0x{:02X} is a state change (a DERIVED UDS frame). \
             Re-run with --confirm to proceed.\n\
             Note: the frame is DERIVED from ISTA disassembly and UNCONFIRMED on a car \
             [verify against capture] — watch the ECU's behaviour afterward.",
            function.name,
            cli.target
        ),
        RunDecision::RunCbs => run_cbs_reset(cli, function).await,
        RunDecision::RunGeneric => run_generic_reset(cli, function).await,
    }
}

/// Execute a derived low-risk reset (statistic/histogram) — caller has confirmed.
///
/// Enters the extended session and sends the function's DERIVED frame (a one-shot
/// `0x2E` write or `0x31` routine); there is no paired read-back for these, so the
/// on-car behavior is the confirmation. The frame is UNCONFIRMED — [verify against
/// capture]. Only reached via [`classify_run`] for a confirmed, derived low-risk
/// function.
async fn run_generic_reset(cli: &Cli, function: &ServiceFunction) -> Result<()> {
    let Some(request) = function.request() else {
        // Unreachable: classify_run only routes Derived functions here.
        bail!("`{}` has no derived frame to run", function.label);
    };
    let (client, _gateway) = connect(cli).await?;
    println!(
        "Running {} (derived frame {request:02X?}) on ECU 0x{:02X} (extended session) …",
        function.name, cli.target
    );
    let response = client
        .run_service_reset(cli.target, request)
        .await
        .context("service reset")?;
    println!("✔ ECU acknowledged the reset (response {response:02X?}).");
    println!(
        "  Frame DERIVED from disassembly [verify against capture] — UNCONFIRMED, and no \
         read-back is wired for this reset. Confirm the effect via ISTA or the ECU's behaviour."
    );
    Ok(())
}

/// Execute a CBS counter reset (the M7 first vertical slice): write, then read back.
///
/// Enters the extended session, writes the CBS reset (`0x2E`), and reads the block
/// back (`0x22`) to confirm the ECU accepted it. The frame is DERIVED from the
/// `CBS_RESET` disassembly — the on-car dashboard reset is the real confirmation.
/// Only reached via [`classify_run`] for a confirmed CBS-reset function.
async fn run_cbs_reset(cli: &Cli, function: &ServiceFunction) -> Result<()> {
    let cbs_id = u8::try_from(function.id)
        .with_context(|| format!("CBS id 0x{:X} is out of byte range", function.id))?;
    let reset = build_cbs_reset_request(cbs_id);
    let read_back = build_cbs_read_request();
    let (client, _gateway) = connect(cli).await?;
    println!(
        "Resetting {} (CBS id 0x{cbs_id:02X}) on ECU 0x{:02X} (extended session, UDS 0x2E) …",
        function.name, cli.target
    );
    let block = client
        .reset_cbs(cli.target, &reset, &read_back)
        .await
        .context("CBS reset")?;
    println!(
        "✔ ECU acknowledged the reset. Read-back ({} byte(s)): {block:02X?}",
        block.len()
    );
    println!(
        "  Frame DERIVED from ISTA data [verify against capture] — confirm on the car that the \
         {} service indicator reset.",
        function.name
    );
    Ok(())
}

/// Connect to the gateway: directly if `--gateway-ip` is set, else by discovery.
async fn connect(cli: &Cli) -> Result<(DiagnosticClient, Option<Gateway>)> {
    let config = ClientConfig {
        port: cli.port,
        tester: TESTER_ADDRESS,
        connect_timeout: Duration::from_millis(cli.connect_timeout),
        read_timeout: Duration::from_millis(cli.timeout),
    };

    if let Some(ip) = cli.gateway_ip {
        println!("Connecting to {ip}:{} (HSFZ, TCP) …", cli.port);
        let client = DiagnosticClient::connect(ip, &config)
            .await
            .context("HSFZ connect failed")?;
        println!("Connected to {ip}. Target ECU 0x{:02X}.", cli.target);
        Ok((client, None))
    } else {
        let bind_desc = cli
            .bind
            .map_or_else(|| "auto-detected link-local".to_string(), |b| b.to_string());
        println!(
            "Discovering gateway (broadcast {} on UDP {CONTROL_PORT}, bind {bind_desc}) …",
            cli.broadcast
        );
        let (client, gateway) = DiagnosticClient::discover_and_connect(
            cli.bind,
            cli.broadcast,
            Duration::from_millis(cli.discovery_wait),
            &config,
        )
        .await
        .context("gateway discovery / connect failed")?;
        println!(
            "Found gateway {}{}. Target ECU 0x{:02X}.",
            gateway.ip,
            gateway
                .vin
                .as_deref()
                .map_or_else(String::new, |v| format!(" (VIN {v})")),
            cli.target
        );
        Ok((client, Some(gateway)))
    }
}

/// The `discover` subcommand: broadcast and list every responder.
async fn run_discover(cli: &Cli) -> Result<()> {
    let bind_ip = match cli.bind {
        Some(ip) => ip,
        None => link_local_bind_ip().context(
            "no link-local interface found; plug in the ENET cable (169.254.x.x) or pass --bind",
        )?,
    };
    println!(
        "Broadcasting HSFZ identification to {}:{CONTROL_PORT} from {bind_ip} …",
        cli.broadcast
    );
    let gateways = discover(
        bind_ip,
        cli.broadcast,
        CONTROL_PORT,
        Duration::from_millis(cli.discovery_wait),
    )
    .await
    .context("discovery failed")?;

    if gateways.is_empty() {
        println!(
            "No replies. Check the ENET cable, your link-local IP (169.254.x.x), \
             and that the car is awake (terminal 15)."
        );
    } else {
        for gateway in &gateways {
            let vin = gateway
                .vin
                .as_deref()
                .map_or_else(String::new, |v| format!("  VIN {v}"));
            println!("← gateway {}{vin}", gateway.ip);
            println!("  raw ({} bytes): {:02X?}", gateway.raw.len(), gateway.raw);
        }
        println!(
            "{} responder(s). Run a read (it auto-connects), or pass --gateway-ip <ip> to pin one.",
            gateways.len()
        );
    }
    print_verify_list();
    Ok(())
}

/// The whole-car scan tuning from the global `--probe-timeout`/`--scan-concurrency`.
fn scan_options(cli: &Cli) -> ScanOptions {
    ScanOptions {
        probe_timeout: Duration::from_millis(cli.probe_timeout),
        concurrency: cli.scan_concurrency,
    }
}

/// The address universe to probe: the DB's ECU map, or a full 0x00..=0xFF sweep.
fn scan_universe(catalog: Option<&Catalog>) -> Vec<u8> {
    match catalog.and_then(|c| c.ecus().ok()) {
        Some(slots) if !slots.is_empty() => slots.iter().map(|s| s.address).collect(),
        _ => (0u8..=0xFF).collect(),
    }
}

/// The DB title for an address, if any (for labeling scanned ECUs).
fn ecu_title(catalog: Option<&Catalog>, address: u8) -> Option<String> {
    catalog
        .and_then(|c| c.ecus().ok())
        .and_then(|slots| slots.into_iter().find(|s| s.address == address))
        .and_then(|slot| slot.title.or(Some(slot.group_name)))
}

/// The `scan` subcommand: find fitted ECUs, then (unless `ecus_only`) their faults.
async fn run_scan(cli: &Cli, ecus_only: bool) -> Result<()> {
    let (client, _gateway) = connect(cli).await?;
    let catalog = open_catalog(&cli.semantic_db);
    let universe = scan_universe(catalog.as_ref());
    if ecus_only {
        let fitted = client.scan_present(&universe, scan_options(cli)).await;
        println!(
            "{} fitted ECU(s) of {} probed:",
            fitted.len(),
            universe.len()
        );
        for f in &fitted {
            let name = ecu_title(catalog.as_ref(), f.address).unwrap_or_default();
            println!(
                "  0x{:02X}  {:>5} ms  {name}",
                f.address,
                f.latency.as_millis()
            );
        }
    } else {
        let faults = client.scan_faults(&universe, scan_options(cli)).await;
        print_scan_faults(&faults, catalog.as_ref());
    }
    print_verify_list();
    Ok(())
}

/// Print a whole-car fault scan: per fitted ECU, its relevant faults + noise count.
fn print_scan_faults(faults: &[EcuFaults], catalog: Option<&Catalog>) {
    let total: usize = faults.iter().map(|e| e.relevant.len()).sum();
    println!(
        "Scanned {} fitted ECU(s); {total} relevant fault(s) total.",
        faults.len()
    );
    for ecu in faults {
        let name = ecu_title(catalog, ecu.address).unwrap_or_default();
        print!("\n  0x{:02X} {name}", ecu.address);
        if let Some(err) = &ecu.error {
            println!("  — ERROR: {err}");
            continue;
        }
        println!(
            "  — {} fault(s){}",
            ecu.relevant.len(),
            if ecu.not_tested > 0 {
                format!(", {} not-tested hidden", ecu.not_tested)
            } else {
                String::new()
            }
        );
        for dtc in &ecu.relevant {
            let flags = status_flags(dtc.status);
            let [hi, mid, lo] = dtc.code;
            println!("      {hi:02X}{mid:02X}{lo:02X}  [{}]", flags.join(", "));
            print_fault_descriptions(catalog, ecu.address, dtc.code);
        }
    }
}

/// The `clear-faults` subcommand: gated single-ECU clear, or whole-car with `--all-ecus`.
async fn run_clear_faults(cli: &Cli, confirm: bool, all_ecus: bool) -> Result<()> {
    // Blast-radius rule (CLAUDE.md): refuse the state change before we connect.
    if !confirm {
        bail!(
            "clear-faults {} — a state change. Re-run with --confirm to proceed.",
            if all_ecus {
                "erases stored codes on EVERY fitted ECU".to_string()
            } else {
                format!("clears stored codes on ECU 0x{:02X}", cli.target)
            }
        );
    }
    let (client, _gateway) = connect(cli).await?;
    if all_ecus {
        let catalog = open_catalog(&cli.semantic_db);
        let universe = scan_universe(catalog.as_ref());
        let fitted = client.scan_present(&universe, scan_options(cli)).await;
        let addrs: Vec<u8> = fitted.iter().map(|f| f.address).collect();
        println!("Clearing faults on {} fitted ECU(s) …", addrs.len());
        for report in client.clear_faults_all(&addrs).await {
            match &report.error {
                Some(err) => println!("  0x{:02X}  ERROR: {err}", report.address),
                None => println!(
                    "  0x{:02X}  {} code(s) cleared — {}",
                    report.address,
                    report.before.len(),
                    if report.verified_clean {
                        "verified clean"
                    } else {
                        "STILL HAS FAULTS (diagnose)"
                    }
                ),
            }
        }
    } else {
        println!(
            "Clearing all DTCs on ECU 0x{:02X} (extended session) …",
            cli.target
        );
        let report = client.clear_faults_verified(cli.target).await;
        if let Some(err) = &report.error {
            bail!("clear failed: {err}");
        }
        println!(
            "✔ Cleared {} code(s); {}.",
            report.before.len(),
            if report.verified_clean {
                "verified clean"
            } else {
                "faults remain — diagnose the underlying cause"
            }
        );
    }
    Ok(())
}

/// Open the semantic catalog, or warn and continue with raw values.
///
/// The semantic DB is BYO-data (ISTA-derived, gitignored) and may be absent, so a
/// missing or unreadable DB downgrades reads to raw output rather than failing.
fn open_catalog(path: &Path) -> Option<Catalog> {
    match Catalog::open(path) {
        Ok(catalog) => Some(catalog),
        Err(e) => {
            eprintln!(
                "note: semantic DB unavailable ({e}). Showing raw codes; build it with \
                 scripts/build-semantic-db.sh or pass --semantic-db <path>."
            );
            None
        }
    }
}

/// Open the SGBD measurement set for proprietary scaling, or warn and skip it.
///
/// The SGBD `.prg` is BYO-data and optional; an absent/unreadable file, or one
/// without `SG_FUNKTIONEN`, downgrades proprietary measurements to raw rather than
/// failing the read.
fn open_measurements(path: Option<&Path>) -> Option<Measurements> {
    let path = path?;
    match Measurements::from_sgbd(path) {
        Ok(measurements) => Some(measurements),
        Err(e) => {
            eprintln!(
                "note: SGBD measurement scaling unavailable ({e}). Showing raw values; \
                 pass --sgbd <ecu>.prg from your EDIABAS/ISTA data."
            );
            None
        }
    }
}

/// Open the SGBD freeze-frame decode definitions, or warn and skip them.
///
/// The SGBD `.prg` is BYO-data and optional; an absent/unreadable file, or one
/// without the freeze-frame tables, leaves the snapshot region raw rather than
/// failing the read.
fn open_freeze_frame_defs(path: Option<&Path>) -> Option<FreezeFrameDefs> {
    let path = path?;
    match FreezeFrameDefs::from_sgbd(path) {
        Ok(defs) => Some(defs),
        Err(e) => {
            eprintln!(
                "note: SGBD freeze-frame decode unavailable ({e}). Showing the raw region; \
                 pass --sgbd <ecu>.prg from your EDIABAS/ISTA data."
            );
            None
        }
    }
}

/// Format bytes as space-separated hex, e.g. `[0x52, 0x05]` → `"52 05"`.
fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Print one fault's freeze-frame detail: descriptions, snapshot, extended, severity.
///
/// Decodes the snapshot/extended regions with the SGBD `defs` (English labels from
/// `catalog` when present); without an SGBD the raw region is shown. The framing is
/// derived from ISO 14229 + disassembly and pending an on-car capture.
fn print_fault_detail(
    target: u8,
    code: [u8; 3],
    detail: &FaultDetailRaw,
    defs: Option<&FreezeFrameDefs>,
    catalog: Option<&Catalog>,
) {
    let [hi, mid, lo] = code;
    println!("Fault {hi:02X}{mid:02X}{lo:02X} on ECU 0x{target:02X}:");
    print_fault_descriptions(catalog, target, code);

    println!("\n  Freeze-frame (snapshot, 19 04):");
    match (&detail.snapshot, defs) {
        (None, _) => println!("    (none stored)"),
        (Some(region), None) => println!(
            "    present but not decoded — pass --sgbd <ecu>.prg ({} raw byte(s): {})",
            region.body.len(),
            hex_bytes(&region.body)
        ),
        (Some(region), Some(defs)) => {
            let decoded = defs.snapshot.decode(region, catalog);
            if decoded.fields.is_empty() {
                println!("    (no fields decoded)");
            }
            for field in &decoded.fields {
                let value = match (field.available, field.value) {
                    (false, _) => "not available".to_string(),
                    (true, Some(v)) => match &field.unit {
                        Some(unit) => format!("{} {unit}", round3(v)),
                        None => round3(v).to_string(),
                    },
                    (true, None) => format!("raw {}", hex_bytes(&field.raw)),
                };
                println!("    {} = {value}", field.label);
            }
            if let Some(tail) = &decoded.undecoded_tail {
                println!(
                    "    (stopped at an unrecognized identifier; {} trailing raw byte(s))",
                    tail.len()
                );
            }
        }
    }

    println!("\n  Extended data (19 06):");
    match (&detail.extended, defs) {
        (None, _) => println!("    (none stored)"),
        (Some(region), None) => println!(
            "    present but not decoded ({} raw byte(s): {})",
            region.body.len(),
            hex_bytes(&region.body)
        ),
        (Some(region), Some(defs)) => {
            let decoded = defs.extended.decode(region);
            if decoded.records.is_empty() {
                println!("    (no records decoded)");
            }
            for record in &decoded.records {
                match record.value {
                    Some(v) => println!("    {} = {v}", record.label),
                    None => println!("    {}", record.label),
                }
            }
            if let Some(tail) = &decoded.undecoded_tail {
                println!(
                    "    (stopped at an unknown record; {} trailing raw byte(s))",
                    tail.len()
                );
            }
        }
    }

    match &detail.severity {
        Some(sev) => println!(
            "\n  Severity (19 09): 0x{:02X} (functional unit 0x{:02X})",
            sev.severity, sev.functional_unit
        ),
        None => println!("\n  Severity (19 09): (none reported)"),
    }
}

/// Print DTCs with decoded ISO status flags and, when available, fault text.
///
/// Real faults are split from "not tested this cycle" catalog noise: by default the
/// noise is only counted; `all` shows every entry.
fn print_faults(dtcs: &[Dtc], target: u8, catalog: Option<&Catalog>, raw: bool, all: bool) {
    let not_tested = dtcs.iter().filter(|d| !d.is_relevant()).count();
    let shown: Vec<&Dtc> = if all {
        dtcs.iter().collect()
    } else {
        dtcs.iter().filter(|d| d.is_relevant()).collect()
    };
    if shown.is_empty() {
        println!("No relevant DTCs on ECU 0x{target:02X}.");
        if not_tested > 0 {
            println!("  ({not_tested} \"not tested this cycle\" entr(y/ies) hidden — pass --all)");
        }
        return;
    }
    println!("{} DTC(s) on ECU 0x{target:02X}:", shown.len());
    for dtc in shown {
        let flags = status_flags(dtc.status);
        let flag_summary = if flags.is_empty() {
            "—".to_string()
        } else {
            flags.join(", ")
        };
        let [hi, mid, lo] = dtc.code;
        println!("\n  {hi:02X}{mid:02X}{lo:02X}  [{flag_summary}]");
        print_fault_descriptions(catalog, target, dtc.code);
        if raw {
            println!("    raw: {dtc}");
        }
    }
    if !all && not_tested > 0 {
        println!("\n  ({not_tested} \"not tested this cycle\" entr(y/ies) hidden — pass --all)");
    }
}

/// Print the per-variant fault descriptions for one DTC, handling DB absence.
fn print_fault_descriptions(catalog: Option<&Catalog>, target: u8, code: [u8; 3]) {
    let Some(catalog) = catalog else {
        return;
    };
    match catalog.describe_dtc(target, code) {
        Ok(descriptions) if !descriptions.is_empty() => {
            for description in &descriptions {
                let title = description
                    .title_en
                    .as_deref()
                    .or(description.title_de.as_deref())
                    .unwrap_or("(no text)");
                let sae = description
                    .saecode
                    .as_deref()
                    .map(|s| format!(" [{s}]"))
                    .unwrap_or_default();
                println!("    {}{sae}: {title}", description.ecu_variant);
            }
        }
        Ok(_) => println!("    (no description in the semantic DB for ECU 0x{target:02X})"),
        Err(e) => println!("    (semantic lookup failed: {e})"),
    }
}

/// Print a DID's decoded name and value; with `raw`, also the underlying bytes.
///
/// Resolution order: a standard OBD-II PID (M5) scales first; else, when an SGBD is
/// loaded and the DID names a `SG_FUNKTIONEN` measurement, its proprietary value is
/// scaled (M6); else the existing named/text/raw behavior applies.
fn print_did_value(
    did: u16,
    value: &[u8],
    target: u8,
    raw: bool,
    measurements: Option<&Measurements>,
) {
    let decoded = did::decode(did, value);
    let proprietary = measurements.and_then(|m| m.scale(did, value));
    let name = proprietary
        .as_ref()
        .map(|m| m.name.as_str())
        .or(decoded.name)
        .unwrap_or("ECU-specific DID");
    println!("DID 0x{did:04X} ({name}) on ECU 0x{target:02X}:");
    if let Some(scaled) = &decoded.scaled {
        // Standard OBD-II / SAE J1979 PID.
        println!("  value: {:.1} {}", scaled.value, scaled.unit);
    } else if let Some(measurement) = &proprietary {
        // BMW-proprietary measurement scaled via SG_FUNKTIONEN.
        println!(
            "  value: {} {}",
            round3(measurement.value),
            measurement.unit
        );
    } else {
        match &decoded.text {
            Some(text) => println!("  value: {text:?}"),
            None => println!(
                "  value: {} byte(s) of binary data (pass --raw to view)",
                value.len()
            ),
        }
    }
    if decoded.name.is_none() && proprietary.is_none() {
        // Not standard and not (yet) resolved via SGBD — see docs/sqlite-findings.md
        // and docs/sgbd-findings.md.
        println!("    (BMW-specific DID — no name/scaling without the SGBD; pass --sgbd to scale)");
    }
    if raw {
        println!("  raw ({} bytes): {value:02X?}", value.len());
    }
}

/// Round to 3 decimals for display, dropping trailing zeros via `f64` formatting.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Print the service-function catalog grouped by category, tagged with risk.
fn print_service_functions(functions: &ServiceFunctions, sgbd: &Path) {
    let name = sgbd.file_name().and_then(|n| n.to_str()).unwrap_or("SGBD");
    if functions.is_empty() {
        println!(
            "No service-function tables in {name}. This ECU's control likely lives in job \
             bytecode (only the DDE ships STELLER / LERNWERTE_RUECK / ABGLEICH tables)."
        );
        return;
    }
    println!("{} service function(s) in {name}:", functions.len());
    for category in [
        Category::CbsReset,
        Category::StatisticReset,
        Category::LearnedValueReset,
        Category::ActuatorControl,
        Category::Calibration,
    ] {
        let group: Vec<_> = functions.by_category(category).collect();
        if group.is_empty() {
            continue;
        }
        println!(
            "\n  {} ({}):",
            category_title(category),
            risk_label(category.risk())
        );
        for function in group {
            println!(
                "    {:12} {:14} {}",
                function.label,
                derivation_tag(function),
                function.name
            );
        }
    }
    println!(
        "\nLow-risk resets with a derived* frame are runnable behind explicit --confirm; \
         low-risk functions marked not-derivable are discovery-only; high-risk actuation and \
         calibration are human-driven only (never autonomous, never over MCP)."
    );
    println!(
        "  * derived = frame read from ISTA disassembly but UNCONFIRMED on a car \
         [verify against capture]. Test low-risk first and confirm the effect before trusting it."
    );
}

/// A short, compact derivation tag for the `service list` output.
fn derivation_tag(function: &ServiceFunction) -> &'static str {
    if function.is_derived() {
        "[derived*]"
    } else {
        "[not-derivable]"
    }
}

/// A human title for a service-function [`Category`].
fn category_title(category: Category) -> &'static str {
    match category {
        Category::CbsReset => "CBS / service-counter reset",
        Category::StatisticReset => "Statistic / histogram reset",
        Category::LearnedValueReset => "Learned-value reset",
        Category::ActuatorControl => "Actuator control",
        Category::Calibration => "Calibration",
    }
}

/// A short blast-radius label for a [`Risk`].
fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low risk",
        Risk::High => "HIGH risk",
    }
}

/// Surface the values the report flags `[verify against capture]` (report Part 6).
fn print_verify_list() {
    println!("\nValues to verify against a capture of your F20 (report Part 6):");
    println!(
        "  • HSFZ LENGTH/ports (TCP {DIAG_PORT} / UDP {CONTROL_PORT}) and control words 01/02/11/12."
    );
    println!(
        "  • Tester 0x{TESTER_ADDRESS:02X} / ZGW 0x{ZGW_ADDRESS:02X}; which TARGETs answer behind the gateway."
    );
    println!("  • 0x11 identification body layout — VIN here is best-effort; confirm the offsets.");
    println!("  • DTC 3-byte code → BMW fault number, and the `59 02` record framing.");
    println!("  • Per-ECU DID set (F190 VIN, 172A IP-config) and BMW-specific NRCs 0xF0–0xFF.");
}

/// Parse a hex byte, with or without a `0x` prefix (addresses are conventionally hex).
fn parse_hex_u8(s: &str) -> Result<u8, String> {
    let t = strip_hex_prefix(s);
    u8::from_str_radix(t, 16).map_err(|e| format!("invalid hex byte `{s}`: {e}"))
}

/// Parse a hex `u16`, with or without a `0x` prefix (DIDs are conventionally hex).
fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let t = strip_hex_prefix(s);
    u16::from_str_radix(t, 16).map_err(|e| format!("invalid hex u16 `{s}`: {e}"))
}

/// Parse a 3-byte DTC from six hex digits, e.g. `240000` or `0x240000`.
fn parse_dtc_arg(s: &str) -> Result<[u8; 3], String> {
    let hex = strip_hex_prefix(s.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid DTC `{s}`: expected 6 hex digits (3 bytes), e.g. 240000"
        ));
    }
    // Each 2-char slice is valid hex (checked above), so the parse cannot fail.
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex digits");
    Ok([byte(0), byte(2), byte(4)])
}

/// Strip an optional `0x`/`0X` prefix.
fn strip_hex_prefix(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use klartext_semantic::Derivation;

    /// A service function with the given category and derivation (no BMW data).
    fn function(category: Category, derivation: Derivation) -> ServiceFunction {
        ServiceFunction {
            label: "X".to_string(),
            name: "X".to_string(),
            category,
            id: 0x01,
            derivation,
        }
    }

    fn derived() -> Derivation {
        Derivation::Derived {
            request: vec![0x2E, 0x5F, 0x84],
            cite: "test",
        }
    }

    fn not_derivable() -> Derivation {
        Derivation::NotDerivable { reason: "test" }
    }

    #[test]
    fn high_risk_is_refused_even_with_confirm_and_even_if_derived() {
        // Risk is the OUTER gate: a HIGH function never runs, whatever its derivation
        // or the confirm flag. (A derived high-risk frame is the dangerous corner case.)
        assert_eq!(
            classify_run(&function(Category::ActuatorControl, not_derivable()), true),
            RunDecision::RefuseHighRisk
        );
        assert_eq!(
            classify_run(&function(Category::Calibration, derived()), true),
            RunDecision::RefuseHighRisk
        );
    }

    #[test]
    fn low_risk_without_a_derived_frame_is_refused() {
        // A frameless low-risk function is discovery-only — never executes.
        assert_eq!(
            classify_run(
                &function(Category::LearnedValueReset, not_derivable()),
                true
            ),
            RunDecision::RefuseNotDerivable
        );
    }

    #[test]
    fn low_risk_derived_without_confirm_needs_confirm() {
        assert_eq!(
            classify_run(&function(Category::CbsReset, derived()), false),
            RunDecision::NeedsConfirm
        );
        assert_eq!(
            classify_run(&function(Category::StatisticReset, derived()), false),
            RunDecision::NeedsConfirm
        );
    }

    #[test]
    fn low_risk_derived_with_confirm_routes_to_the_right_executor() {
        assert_eq!(
            classify_run(&function(Category::CbsReset, derived()), true),
            RunDecision::RunCbs
        );
        assert_eq!(
            classify_run(&function(Category::StatisticReset, derived()), true),
            RunDecision::RunGeneric
        );
    }
}
