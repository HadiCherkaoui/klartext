//! In-process integration tests driving the MCP tools directly (no real car).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;
use klartext_mcp::dto::{
    ClearAllFaultsRequest, ClearFaultsRequest, ConnectRequest, ListMeasurementsRequest,
    ListServiceFunctionsRequest, ReadAllFaultsRequest, ReadDataRequest, ReadFaultsRequest,
    ScanEcusRequest,
};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::net::TcpListener;

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
    assert!(
        tools.contains(&"disconnect".to_string()),
        "tools: {tools:?}"
    );
}

/// Build a synthetic semantic DB (no BMW data) matching the extract schema.
fn fixture_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
         CREATE TABLE ecu (address INT, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
         INSERT INTO dtc VALUES (64,'fem_20',14222346,NULL,'BEISPIEL Fehler A','EXAMPLE fault A: bus, no communication');
         INSERT INTO dtc VALUES (64,'fem_21',14222346,NULL,'BEISPIEL Fehler B','EXAMPLE fault B: bus communication fault');
         INSERT INTO ecu VALUES (16,'zgw_x','d_0010','Gateway',NULL);
         INSERT INTO ecu VALUES (18,'dde_x','d_0012','Digital Diesel Electronics',NULL);
         INSERT INTO ecu VALUES (64,'fem_20','d_0040','Front Electronic Module',NULL);
         -- 0x18 is in the model map but absent on the mock car (probe stays silent).
         INSERT INTO ecu VALUES (24,'egs_x','d_0018','Transmission',NULL);",
    )
    .unwrap();
    (dir, path)
}

/// A server config pointed at a fixture DB (no gateway set).
fn config_with_db(path: &Path) -> ServerConfig {
    ServerConfig::parse_from(["klartext-mcp", "--semantic-db", path.to_str().unwrap()])
}

#[tokio::test]
async fn list_ecus_names_ecus_from_the_db() {
    let (_dir, path) = fixture_db();
    let server = KlartextServer::new(config_with_db(&path));
    let result = server.list_ecus().await.unwrap();
    assert!(result.0.db_available);
    assert!(result.0.db_error.is_none());
    // 0x40 is the FEM group d_0040 with its DB title and variant candidates.
    let fem = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(fem.group_name, "d_0040");
    assert_eq!(fem.title.as_deref(), Some("Front Electronic Module"));
    assert!(fem.variants.iter().any(|v| v == "fem_20"));
    // 0x12 is the engine group d_0012. No hardcoded "DME"/"CAS" aliases exist.
    let dde = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x12")
        .unwrap();
    assert_eq!(dde.group_name, "d_0012");
}

#[tokio::test]
async fn list_ecus_without_db_is_empty() {
    let server = KlartextServer::new(test_config());
    let result = server.list_ecus().await.unwrap();
    assert!(!result.0.db_available);
    assert!(result.0.ecus.is_empty());
}

/// The recorded, ordered UDS payloads a mock gateway has received (keepalives excluded).
type FrameLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// The ECUs the mock car answers for; 0x18 (in the DB map) is deliberately absent.
const MOCK_PRESENT: &[u8] = &[0x10, 0x12, 0x40];

/// A loopback mock gateway with several ECUs demultiplexed over one connection.
///
/// Answers a presence probe (`3E 00`), VIN/DTC/PID reads, the dynamic-measurement
/// sequence, and the extended-session + standard-clear handshakes; ignores
/// keepalives. Only [`MOCK_PRESENT`] addresses answer — others stay silent so a
/// scan skips them. Every reply swaps SRC/TGT (as the real gateway does), so the
/// client's demux routes it by the answering ECU's address. Each ECU tracks a
/// "cleared" flag so a post-clear re-read comes back clean. Every non-keepalive
/// UDS payload is recorded in the returned log for exact-frame assertions.
async fn spawn_mock_gateway() -> (std::net::SocketAddr, FrameLog) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: FrameLog = std::sync::Arc::default();
    let shared = std::sync::Arc::clone(&log);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = std::sync::Arc::clone(&shared);
            tokio::spawn(async move {
                let mut cleared: std::collections::HashSet<u8> = Default::default();
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    if frame.payload.as_slice() == [0x3E, 0x80] {
                        continue; // keepalive — unlogged (timing-dependent), no reply
                    }
                    if !MOCK_PRESENT.contains(&ecu) {
                        continue; // absent ECU — silence (a probe there times out)
                    }
                    log.lock().unwrap().push(frame.payload.clone());
                    let reply = match frame.payload.as_slice() {
                        [0x3E, 0x00] => vec![0x7E, 0x00], // presence probe
                        [0x22, 0xF1, 0x90] => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(b"WBA3B5C50EK123456");
                            uds
                        }
                        // OBDDataIdentifier for PID 0x0C (engine RPM): 0D 48 -> 850 rpm.
                        [0x22, 0xF4, 0x0C] => vec![0x62, 0xF4, 0x0C, 0x0D, 0x48],
                        // M6 Part B: the DDE "selektiv lesen" sequence for engine
                        // temp (id 0x4BC3, u16), DERIVED from the d72n47a0
                        // disassembly (docs/sgbd-findings.md §7a): clear, define
                        // F303 from source DID 4BC3, then read F303 -> raw 0E 2F
                        // (u16 3631 * 0.1 - 273.14 = 89.96 degC).
                        [0x2C, 0x03, 0xF3, 0x03] => vec![0x6C, 0x03, 0xF3, 0x03],
                        [0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02] => {
                            vec![0x6C, 0x01, 0xF3, 0x03]
                        }
                        [0x22, 0xF3, 0x03] => vec![0x62, 0xF3, 0x03, 0x0E, 0x2F],
                        // After a clear, this ECU reads clean.
                        [0x19, 0x02, _mask] if cleared.contains(&ecu) => vec![0x59, 0x02, 0xFF],
                        // One relevant DTC (D9 04 0A, status 0x08) + one "not tested
                        // this cycle" catalog entry (AA BB CC, status 0x40) to
                        // exercise the relevance partition.
                        [0x19, 0x02, _mask] => vec![
                            0x59, 0x02, 0xFF, 0xD9, 0x04, 0x0A, 0x08, 0xAA, 0xBB, 0xCC, 0x40,
                        ],
                        // Extended session + the standard clear-all (M9 Part B).
                        [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
                        [0x14, 0xFF, 0xFF, 0xFF] => {
                            cleared.insert(ecu);
                            vec![0x54]
                        }
                        _ => continue,
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, reply); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    (addr, log)
}

/// The BYO SGBD directory (never committed): `data/Testmodule(1)/Ecu` in the workspace.
fn sgbd_test_dir() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../data/Testmodule(1)/Ecu")
        .to_str()
        .unwrap()
        .to_string()
}

/// A server config pointed at the mock gateway + a fixture DB.
fn config_for_mock(addr: std::net::SocketAddr, db: &Path) -> ServerConfig {
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
    let (addr, _frames) = spawn_mock_gateway().await;
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

#[tokio::test]
async fn read_faults_decodes_flags_and_descriptions() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // 0x40 is in the fixture's dtc + ecu tables (group d_0040).
    let result = server
        .read_faults(Parameters(ReadFaultsRequest {
            ecu: "0x40".to_string(),
            include_not_tested: false,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.address, "0x40");
    // Only the relevant fault is shown; the "not tested this cycle" entry is counted.
    assert_eq!(result.0.count, 1);
    assert_eq!(result.0.not_tested_count, 1);
    let fault = &result.0.faults[0];
    assert_eq!(fault.code_hex, "D9040A");
    assert_eq!(fault.status_hex, "08");
    assert_eq!(fault.status_flags, vec!["confirmedDTC".to_string()]);
    assert!(result.0.db_available);
    // Two variants share the code at address 0x40.
    assert_eq!(fault.descriptions.len(), 2);
    assert!(fault.descriptions.iter().any(|d| {
        d.text
            .as_deref()
            .is_some_and(|t| t.contains("EXAMPLE fault A"))
    }));
}

#[tokio::test]
async fn read_faults_without_connect_errors_clearly() {
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_with_db(&db));
    let result = server
        .read_faults(Parameters(ReadFaultsRequest {
            ecu: "0x40".to_string(),
            include_not_tested: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a not-connected error, got Ok");
    };
    assert!(err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn read_data_decodes_vin() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x10".to_string(),
            did: Some("F190".to_string()),
            name: None,
            variant: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "F190");
    assert_eq!(result.0.name.as_deref(), Some("VIN"));
    assert_eq!(result.0.value_text.as_deref(), Some("WBA3B5C50EK123456"));
    // A non-PID identification DID carries no engineering value.
    assert_eq!(result.0.scaled_value, None);
    assert_eq!(result.0.unit, None);
}

#[tokio::test]
async fn read_data_scales_a_standard_pid() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // 0xF40C = OBDDataIdentifier for engine RPM; the mock returns 0D 48 -> 850 rpm.
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x10".to_string(),
            did: Some("F40C".to_string()),
            name: None,
            variant: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "F40C");
    assert_eq!(result.0.name.as_deref(), Some("Engine RPM"));
    assert_eq!(result.0.unit.as_deref(), Some("rpm"));
    let value = result.0.scaled_value.expect("standard PID should scale");
    assert!((value - 850.0).abs() < 1e-6, "got {value}");
    // Raw bytes are always present alongside the scaled value.
    assert_eq!(result.0.raw_hex, "0D 48");
}

// M6 Part B: the full dynamic-measurement path — define -> read -> scale — over
// real loopback frames, with NO BYO data. The engine-temp formula is public and
// the 2C/22 frames are DERIVED from the d72n47a0 disassembly (docs/sgbd-findings.md
// §7a). Exercises the same client + semantic code the read_data tool runs for a
// dynamic SG_FUNKTIONEN measurement, without the proprietary `.prg`.
#[tokio::test]
async fn dynamic_measurement_defines_reads_and_scales() {
    use klartext_client::{ClientConfig, DiagnosticClient};
    use klartext_semantic::{DataType, Measurement, build_read_request};

    let (addr, _frames) = spawn_mock_gateway().await;
    let config = ClientConfig {
        port: addr.port(),
        ..ClientConfig::default()
    };
    let client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

    // The engine-temperature measurement (id 0x4BC3, u16, SERVICE "22;2C") — the
    // public scaling formula, not BMW data.
    let measurement = Measurement {
        arg: "ITMOT".to_string(),
        id: 0x4BC3,
        result_name: "STAT_MOTORTEMPERATUR_WERT".to_string(),
        description: "Motortemperatur".to_string(),
        unit: "degC".to_string(),
        datatype: DataType::U16,
        mul: 0.1,
        div: 1.0,
        add: -273.14,
        sg_adr: "12".to_string(),
        service: "22;2C".to_string(),
    };
    assert!(measurement.is_dynamic());

    // define -> read over the wire, then scale via Part A.
    let requests = build_read_request(&measurement);
    let raw = client
        .read_dynamic_measurement(0x12, &requests)
        .await
        .unwrap();
    assert_eq!(raw, vec![0x0E, 0x2F]); // raw bytes preserved
    let scaled = measurement.scaled(&raw).expect("scales");
    assert_eq!(scaled.name, "Motortemperatur");
    assert_eq!(scaled.unit, "degC");
    assert!((scaled.value - 89.96).abs() < 0.01, "got {}", scaled.value);
}

// M6 Part B: a proprietary DYNAMIC measurement (SERVICE "22;2C") read through the
// MCP read_data tool with the real SGBD — the server runs the 0x2C define + 0x22
// read sequence (answered by the mock) then scales. Ignored by default (needs the
// BYO `.prg`); run with `--ignored`. Offline precursor to the on-car manual step.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn read_data_scales_a_proprietary_measurement() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        &sgbd_dir,
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: Some("4BC3".to_string()),
            name: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.0.name.as_deref(), Some("Motortemperatur"));
    assert_eq!(result.0.unit.as_deref(), Some("degC"));
    let value = result
        .0
        .scaled_value
        .expect("proprietary value should scale");
    assert!((value - 89.96).abs() < 0.01, "got {value}");
    assert_eq!(result.0.raw_hex, "0E 2F");
}

#[tokio::test]
async fn read_data_requires_exactly_one_of_did_or_name() {
    // The did/name contract is checked before any connection is touched.
    let server = KlartextServer::new(test_config());
    for (did, name) in [(None, None), (Some("F190"), Some("ITMOT"))] {
        let result = server
            .read_data(Parameters(ReadDataRequest {
                ecu: "0x10".to_string(),
                did: did.map(String::from),
                name: name.map(String::from),
                variant: None,
            }))
            .await;
        let Err(err) = result else {
            panic!("expected an error for did={did:?} name={name:?}, got Ok");
        };
        assert!(err.message.contains("exactly one"), "{}", err.message);
    }
}

#[tokio::test]
async fn read_data_with_an_unservable_variant_errors_loudly() {
    // An explicit `variant` the server cannot load (here: no --sgbd-dir) is a
    // configuration error, not a silent degrade-to-raw — the caller asked for
    // scaled values and must learn why they cannot have them.
    let server = KlartextServer::new(test_config());
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: Some("4BC3".to_string()),
            name: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a no-SGBD error, got Ok");
    };
    assert!(err.message.contains("no SGBD"), "{}", err.message);
}

#[tokio::test]
async fn read_data_by_name_requires_a_variant() {
    // A name can only be resolved through an SGBD catalog, which `variant` picks.
    let server = KlartextServer::new(test_config());
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: None,
            name: Some("Motortemperatur".to_string()),
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without variant, got Ok");
    };
    assert!(err.message.contains("variant"), "{}", err.message);
}

// M9 Part A discover→read: a measurement found via list_measurements is read by
// NAME — no DID knowledge needed. The server resolves "Motortemperatur" through
// the real SGBD to id 0x4BC3, runs the dynamic 2C/22 sequence (answered by the
// mock), and returns the scaled value + unit. Ignored by default (BYO `.prg`).
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn read_data_reads_a_measurement_by_name() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        &sgbd_dir,
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: None,
            name: Some("Motortemperatur".to_string()),
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "4BC3");
    assert_eq!(result.0.name.as_deref(), Some("Motortemperatur"));
    assert_eq!(result.0.unit.as_deref(), Some("degC"));
    let value = result.0.scaled_value.expect("scales by name");
    assert!((value - 89.96).abs() < 0.01, "got {value}");
}

#[tokio::test]
async fn read_data_rejects_bad_did_hex() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x10".to_string(),
            did: Some("ZZZZ".to_string()),
            name: None,
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an invalid-DID error, got Ok");
    };
    assert!(err.message.contains("invalid DID hex"), "{}", err.message);
}

#[tokio::test]
async fn clear_faults_refuses_without_confirm() {
    // The confirmation gate is checked before anything else — even before the
    // "not connected" check — so a refusal never touches the car.
    let server = KlartextServer::new(test_config());
    let result = server
        .clear_faults(Parameters(ClearFaultsRequest {
            ecu: "0x40".to_string(),
            confirm: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a refusal without confirm, got Ok");
    };
    assert!(err.message.contains("confirm"), "{}", err.message);
    assert!(err.message.contains("freeze-frame"), "{}", err.message);
    assert!(err.message.contains("readiness"), "{}", err.message);
    assert!(!err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn clear_faults_confirmed_but_disconnected_errors_clearly() {
    let server = KlartextServer::new(test_config());
    let result = server
        .clear_faults(Parameters(ClearFaultsRequest {
            ecu: "0x40".to_string(),
            confirm: true,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a not-connected error, got Ok");
    };
    assert!(err.message.contains("not connected"), "{}", err.message);
}

// M9 Part B: the confirmed clear over the wire — and the refined safety invariant,
// behaviorally: every frame this write path sends is ISO-standard UDS (DTC pre-read,
// extended session, ClearDiagnosticInformation). No derived/proprietary frame, ever.
#[tokio::test]
async fn clear_faults_with_confirm_clears_and_sends_only_standard_frames() {
    let (addr, frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .clear_faults(Parameters(ClearFaultsRequest {
            ecu: "0x40".to_string(),
            confirm: true,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.address, "0x40");
    assert!(result.0.cleared);
    // The pre-read records EVERY stored code discarded — relevant and not-tested.
    assert_eq!(
        result.0.codes_cleared,
        vec!["D9040A".to_string(), "AABBCC".to_string()]
    );
    assert_eq!(result.0.count, 2);
    assert!(result.0.note.contains("read_faults"), "{}", result.0.note);

    let frames = frames.lock().unwrap().clone();
    assert_eq!(
        frames,
        vec![
            vec![0x22, 0xF1, 0x90],       // connect: VIN read (from the gateway)
            vec![0x19, 0x02, 0xFF],       // pre-read: record what will be discarded
            vec![0x10, 0x03],             // extended session (required before a clear)
            vec![0x14, 0xFF, 0xFF, 0xFF], // standard clear-all (M2 path, no new frame)
        ]
    );
}

// The refined M9 surface invariant: read tools plus exactly ONE standard,
// non-physical, confirmation-gated write (clear_faults). NO physical actuation and
// NO service-function/derived-unconfirmed-frame execution may ever appear as a
// tool — those stay human-executed in the CLI. (The wire-level half of the
// invariant — only standard frames leave the write path — is asserted by
// `clear_faults_with_confirm_clears_and_sends_only_standard_frames`.)
#[test]
fn advertises_exactly_the_refined_tool_surface() {
    let server = KlartextServer::new(test_config());
    let mut tools = server.advertised_tools();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "clear_all_faults".to_string(),
            "clear_faults".to_string(),
            "connect".to_string(),
            "disconnect".to_string(),
            "list_ecus".to_string(),
            "list_measurements".to_string(),
            "list_service_functions".to_string(),
            "read_all_faults".to_string(),
            "read_data".to_string(),
            "read_faults".to_string(),
            "scan_ecus".to_string(),
        ]
    );
    // `list_service_functions` LISTS control functions but must never gain the
    // power to run one: any verb that would actuate, execute, or otherwise mutate
    // beyond the one allowed clear is forbidden as a substring of every tool name.
    for forbidden in [
        "actuat",
        "io_control",
        "run",
        "execut",
        "routine",
        "regen",
        "calibrat",
        "write",
        "code",
        "coding",
        "reset",
        "flash",
    ] {
        assert!(
            !tools.iter().any(|t| t.contains(forbidden)),
            "forbidden tool present: {forbidden}"
        );
    }
    // "clear" appears only on the two confirmation-gated clears (per-ECU + whole-car),
    // both standard UDS 0x14 — never on an actuation/coding verb.
    let mut clears: Vec<&String> = tools.iter().filter(|t| t.contains("clear")).collect();
    clears.sort();
    assert_eq!(clears, vec!["clear_all_faults", "clear_faults"]);
}

#[tokio::test]
async fn list_measurements_requires_an_sgbd_dir() {
    // Without --sgbd-dir there is no measurement catalog to serve; the tool errors
    // clearly rather than inventing entries.
    let server = KlartextServer::new(test_config());
    let result = server
        .list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            search: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without --sgbd-dir, got Ok");
    };
    assert!(err.message.contains("no SGBD"), "{}", err.message);
}

// M9 Part A over the real DDE SGBD: the diesel-useful live-data set — oil temp,
// coolant temp, DPF soot/ash mass, regeneration status, engine RPM — surfaces from
// SG_FUNKTIONEN by name, and the huge catalog is capped with an explicit note (no
// silent truncation). Ignored by default (BYO data); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn list_measurements_lists_the_real_dde_catalog() {
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from(["klartext-mcp", "--sgbd-dir", &sgbd_dir]);
    let server = KlartextServer::new(config);
    let list = |search: Option<&str>| {
        server.list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            search: search.map(String::from),
        }))
    };

    // Unfiltered: the DDE defines ~1800 measurements; the reply caps and says so.
    let all = list(None).await.unwrap();
    assert!(all.0.total > 1000, "got {}", all.0.total);
    assert_eq!(all.0.count, all.0.measurements.len());
    assert!(all.0.count < all.0.total, "expected the cap to apply");
    assert!(all.0.note.contains("search"), "{}", all.0.note);

    // Oil temperature: id 4517 (ITOEL), scaled in degC.
    let oil = list(Some("Öltemperatur")).await.unwrap();
    let itoel = oil
        .0
        .measurements
        .iter()
        .find(|m| m.arg == "ITOEL")
        .expect("ITOEL in the oil-temperature listing");
    assert_eq!(itoel.id_hex, "4517");
    assert_eq!(itoel.unit, "degC");
    assert_eq!(itoel.name, "gefilterte Öltemperatur");
    // The listed ECU address round-trips into read_data/read_faults' `ecu` form.
    assert_eq!(itoel.ecu_address, "0x12");

    // Coolant, DPF soot mass, regeneration status, engine RPM — all discoverable.
    for (search, arg) in [
        ("Kühlmitteltemperatur", "ITKUM"),
        ("Rußmasse", "IMRUP"),
        ("Regenerationsanforderung", "PFltRgn_numRgn"),
        ("Motordrehzahl", "Nkw"),
    ] {
        let found = list(Some(search)).await.unwrap();
        assert!(
            found.0.measurements.iter().any(|m| m.arg == arg),
            "search '{search}' should surface {arg}"
        );
    }
}

#[tokio::test]
async fn list_service_functions_requires_an_sgbd_dir() {
    // With no --sgbd-dir configured, the catalog cannot be served and the tool errors
    // clearly rather than executing or panicking.
    let server = KlartextServer::new(test_config());
    let result = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            risk: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without --sgbd-dir, got Ok");
    };
    assert!(err.message.contains("no SGBD"), "{}", err.message);
}

// The read-only service-function listing over the real DDE SGBD. Ignored by default
// (needs the BYO `.prg`); run with `--ignored`. Asserts the catalog, risk tiers, and
// derivation status — and that NO frame bytes are exposed (list-only).
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn list_service_functions_lists_the_real_dde_catalog() {
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from(["klartext-mcp", "--sgbd-dir", &sgbd_dir]);
    let server = KlartextServer::new(config);

    // Full catalog: 160 functions (156 control-table rows + 4 derived resets).
    let all = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            risk: None,
        }))
        .await
        .unwrap();
    assert_eq!(all.0.count, all.0.functions.len());
    assert!(all.0.count > 100, "got {}", all.0.count);

    // The engine-oil CBS reset: low-risk, derived (unconfirmed), runnable in the CLI.
    let oil = all.0.functions.iter().find(|f| f.label == "Oel").unwrap();
    assert_eq!(oil.risk, "low");
    assert_eq!(oil.derivation, "derived-unconfirmed");
    assert!(oil.runnable_in_cli);
    assert!(oil.citation.as_deref().unwrap().contains("CBS_RESET"));

    // A throttle actuator: high-risk, never runnable via this surface.
    let dro = all.0.functions.iter().find(|f| f.label == "DRO").unwrap();
    assert_eq!(dro.risk, "high");
    assert!(!dro.runnable_in_cli);

    // The risk filter narrows to low-risk only.
    let low = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            risk: Some("low".to_string()),
        }))
        .await
        .unwrap();
    assert!(low.0.functions.iter().all(|f| f.risk == "low"));
    assert!(low.0.count < all.0.count);
}

// ── Whole-car tools (scan_ecus / read_all_faults / clear_all_faults) ──────────

#[tokio::test]
async fn scan_ecus_finds_only_the_fitted_set() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .scan_ecus(Parameters(ScanEcusRequest { rescan: false }))
        .await
        .unwrap();
    // The DB map is {0x10,0x12,0x40,0x18}; the mock answers only the first three,
    // so 0x18 must be skipped (fast, not hung).
    let addrs: Vec<&str> = result
        .0
        .ecus
        .iter()
        .map(|e| e.address_hex.as_str())
        .collect();
    assert_eq!(addrs, ["0x10", "0x12", "0x40"]);
    assert_eq!(result.0.probed, 4);
    // Names come from the DB.
    let fem = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(fem.title.as_deref(), Some("Front Electronic Module"));
}

#[tokio::test]
async fn read_all_faults_reads_every_fitted_ecu_and_partitions() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_all_faults(Parameters(ReadAllFaultsRequest { rescan: false }))
        .await
        .unwrap();
    // One EcuFaults entry per fitted ECU; each has the one relevant fault and one
    // not-tested entry counted.
    assert_eq!(result.0.ecus.len(), 3);
    assert_eq!(result.0.total_relevant, 3);
    for ecu in &result.0.ecus {
        assert_eq!(ecu.faults.len(), 1, "{}", ecu.address_hex);
        assert_eq!(ecu.faults[0].code_hex, "D9040A");
        assert_eq!(ecu.not_tested_count, 1);
        assert!(ecu.error.is_none());
    }
}

#[tokio::test]
async fn clear_all_faults_refuses_without_confirm() {
    // The whole-car confirmation gate refuses before touching anything.
    let server = KlartextServer::new(test_config());
    let result = server
        .clear_all_faults(Parameters(ClearAllFaultsRequest {
            confirm: false,
            rescan: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a refusal without confirm, got Ok");
    };
    assert!(err.message.contains("whole car"), "{}", err.message);
    assert!(err.message.contains("freeze-frame"), "{}", err.message);
    assert!(!err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn clear_all_faults_confirmed_clears_every_fitted_ecu_and_verifies() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    // Scan first so the fitted list is cached.
    server
        .scan_ecus(Parameters(ScanEcusRequest { rescan: false }))
        .await
        .unwrap();

    let result = server
        .clear_all_faults(Parameters(ClearAllFaultsRequest {
            confirm: true,
            rescan: false,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.ecus.len(), 3);
    assert_eq!(result.0.cleared_clean, 3);
    for ecu in &result.0.ecus {
        assert!(ecu.verified_clean, "{}", ecu.address_hex);
        // Every ECU stored both codes before the clear (the discard record).
        assert_eq!(
            ecu.codes_before,
            vec!["D9040A".to_string(), "AABBCC".to_string()]
        );
        assert!(ecu.error.is_none());
    }
}
