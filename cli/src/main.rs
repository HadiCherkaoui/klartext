//! klartext CLI — auto-discover a BMW F-series gateway over HSFZ, then read fault
//! codes and data identifiers from a chosen ECU, or clear faults behind explicit
//! confirmation. Faults and identifiers are decoded to human text via the
//! ISTA-derived semantic database (`klartext-semantic`).
//!
//! The default connect path is auto-discovery: the tool broadcasts an HSFZ
//! identification request, finds the gateway, and connects. `--gateway-ip` skips
//! discovery and connects directly (the M1 path). Reads are autonomous-safe;
//! `clear-faults` is a state change and refuses to run without `--confirm`.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use klartext_client::{ClientConfig, DEFAULT_BROADCAST, DiagnosticClient, Gateway};
use klartext_hsfz::{
    CONNECT_TIMEOUT_DEFAULT_MS, CONTROL_PORT, DIAG_PORT, TESTER_ADDRESS, ZGW_ADDRESS, discover,
    link_local_bind_ip,
};
use klartext_semantic::{Catalog, Measurements, build_read_request, did, dtc::status_flags};
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

    /// Target ECU logical address (hex, e.g. 10 = ZGW, 12 = DME).
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
    /// Read fault codes (DTCs) from the target ECU.
    ReadFaults {
        /// DTC status mask (hex): which status bits to match. FF = all stored.
        #[arg(long, value_parser = parse_hex_u8, default_value = "0xFF")]
        mask: u8,
        /// Also print the raw 3-byte code / status byte.
        #[arg(long)]
        raw: bool,
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
    },
    /// Send a TesterPresent to the target ECU (a connectivity check).
    TesterPresent,
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
        Command::ReadFaults { mask, raw } => {
            let (mut client, _gateway) = connect(&cli).await?;
            let dtcs = client.read_dtcs(*mask).await.context("reading DTCs")?;
            let catalog = open_catalog(&cli.semantic_db);
            print_faults(&dtcs, cli.target, catalog.as_ref(), *raw);
            print_verify_list();
        }
        Command::ReadDid { did, raw } => {
            let (mut client, _gateway) = connect(&cli).await?;
            let measurements = open_measurements(cli.sgbd.as_deref());
            // M6 Part B: a dynamic SG_FUNKTIONEN measurement (SERVICE "22;2C") reads
            // via the 0x2C define + 0x22 read sequence; a static DID is a plain 0x22
            // read. Either way the requested id is shown (not the dynamic 0xF303).
            let (got_did, value) = match measurements.as_ref().and_then(|m| m.get(*did)) {
                Some(measurement) if measurement.is_dynamic() => {
                    let requests = build_read_request(measurement);
                    let value = client
                        .read_dynamic_measurement(&requests)
                        .await
                        .context("reading dynamic measurement")?;
                    (*did, value)
                }
                _ => client.read_did(*did).await.context("reading DID")?,
            };
            print_did_value(got_did, &value, cli.target, *raw, measurements.as_ref());
            print_verify_list();
        }
        Command::ClearFaults { confirm } => {
            // Blast-radius rule (CLAUDE.md): refuse the state change before we
            // even connect unless the user explicitly confirmed it.
            if !confirm {
                bail!(
                    "clear-faults clears stored fault codes on ECU 0x{:02X} — a state change. \
                     Re-run with --confirm to proceed.",
                    cli.target
                );
            }
            let (mut client, _gateway) = connect(&cli).await?;
            println!(
                "Clearing all DTCs on ECU 0x{:02X} (extended session) …",
                cli.target
            );
            client.clear_all_dtcs().await.context("clearing DTCs")?;
            println!("✔ Cleared. Re-read faults to confirm.");
        }
        Command::TesterPresent => {
            let (mut client, _gateway) = connect(&cli).await?;
            client
                .tester_present()
                .await
                .context("sending TesterPresent")?;
            println!("✔ TesterPresent acknowledged by ECU 0x{:02X}.", cli.target);
        }
    }
    Ok(())
}

/// Connect to the gateway: directly if `--gateway-ip` is set, else by discovery.
async fn connect(cli: &Cli) -> Result<(DiagnosticClient, Option<Gateway>)> {
    let config = ClientConfig {
        port: cli.port,
        ecu: cli.target,
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

/// Print DTCs with decoded ISO status flags and, when available, fault text.
fn print_faults(dtcs: &[Dtc], target: u8, catalog: Option<&Catalog>, raw: bool) {
    if dtcs.is_empty() {
        println!("No DTCs on ECU 0x{target:02X} for the requested status mask.");
        return;
    }
    println!("{} DTC(s) on ECU 0x{target:02X}:", dtcs.len());
    for dtc in dtcs {
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

/// Strip an optional `0x`/`0X` prefix.
fn strip_hex_prefix(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}
