//! In-process integration tests driving the MCP tools directly (no real car).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;
use klartext_mcp::dto::{
    ConnectRequest, ListServiceFunctionsRequest, ReadDataRequest, ReadFaultsRequest,
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
fn config_with_db(path: &Path) -> ServerConfig {
    ServerConfig::parse_from(["klartext-mcp", "--semantic-db", path.to_str().unwrap()])
}

#[tokio::test]
async fn list_ecus_merges_builtin_and_db() {
    let (_dir, path) = fixture_db();
    let server = KlartextServer::new(config_with_db(&path));
    let result = server.list_ecus().await.unwrap();
    assert!(result.0.db_available);
    // 0x40 is both a builtin alias (CAS) and a DB group (d_0040).
    let cas = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(cas.source, "builtin+db");
    assert_eq!(cas.group_name.as_deref(), Some("d_0040"));
    assert!(cas.names.iter().any(|n| n == "CAS"));
    // 0x12 has both the DME alias and the d_0012 group.
    let dme = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x12")
        .unwrap();
    assert_eq!(dme.source, "builtin+db");
}

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
                            let reply = HsfzFrame::diagnostic(0x10, 0xF4, uds);
                            let _ = write_frame(&mut stream, &reply).await;
                        }
                        [0x22, 0xF4, 0x0C] => {
                            // OBDDataIdentifier for PID 0x0C (engine RPM): 0D 48 -> 850 rpm.
                            let uds = vec![0x62, 0xF4, 0x0C, 0x0D, 0x48];
                            let reply = HsfzFrame::diagnostic(0x10, 0xF4, uds);
                            let _ = write_frame(&mut stream, &reply).await;
                        }
                        // M6 Part B: the DDE "selektiv lesen" sequence for engine
                        // temp (id 0x4BC3, u16), DERIVED from the d72n47a0
                        // disassembly (docs/sgbd-findings.md §7a): clear, define
                        // F303 from source DID 4BC3, then read F303 -> raw 0E 2F.
                        [0x2C, 0x03, 0xF3, 0x03] => {
                            let reply =
                                HsfzFrame::diagnostic(0x10, 0xF4, vec![0x6C, 0x03, 0xF3, 0x03]);
                            let _ = write_frame(&mut stream, &reply).await;
                        }
                        [0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02] => {
                            let reply =
                                HsfzFrame::diagnostic(0x10, 0xF4, vec![0x6C, 0x01, 0xF3, 0x03]);
                            let _ = write_frame(&mut stream, &reply).await;
                        }
                        [0x22, 0xF3, 0x03] => {
                            // raw 0E 2F -> u16 3631 * 0.1 - 273.14 = 89.96 degC.
                            let reply = HsfzFrame::diagnostic(
                                0x10,
                                0xF4,
                                vec![0x62, 0xF3, 0x03, 0x0E, 0x2F],
                            );
                            let _ = write_frame(&mut stream, &reply).await;
                        }
                        [0x19, 0x02, _mask] => {
                            // one DTC: code D9 04 0A (== 14222346), status 0x08 (confirmed).
                            let uds = vec![0x59, 0x02, 0xFF, 0xD9, 0x04, 0x0A, 0x08];
                            let reply = HsfzFrame::diagnostic(0x10, 0xF4, uds);
                            let _ = write_frame(&mut stream, &reply).await;
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
        .read_faults(Parameters(ReadFaultsRequest {
            ecu: "0x40".to_string(),
        }))
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
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a not-connected error, got Ok");
    };
    assert!(err.message.contains("not connected"), "{}", err.message);
}

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
    let addr = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // 0xF40C = OBDDataIdentifier for engine RPM; the mock returns 0D 48 -> 850 rpm.
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "ZGW".to_string(),
            did: "F40C".to_string(),
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

    let addr = spawn_mock_gateway().await;
    let config = ClientConfig {
        port: addr.port(),
        ecu: 0x12,
        ..ClientConfig::default()
    };
    let mut client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

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
    let raw = client.read_dynamic_measurement(&requests).await.unwrap();
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
    let addr = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/Testmodule(1)/Ecu");
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        sgbd_dir.to_str().unwrap(),
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: "4BC3".to_string(),
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
async fn read_data_rejects_bad_did_hex() {
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
            did: "ZZZZ".to_string(),
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an invalid-DID error, got Ok");
    };
    assert!(err.message.contains("invalid DID hex"), "{}", err.message);
}

#[test]
fn advertises_exactly_the_six_read_only_tools() {
    let server = KlartextServer::new(test_config());
    let mut tools = server.advertised_tools();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "connect".to_string(),
            "disconnect".to_string(),
            "list_ecus".to_string(),
            "list_service_functions".to_string(),
            "read_data".to_string(),
            "read_faults".to_string(),
        ]
    );
    // No mutating tool may ever appear on the MCP surface. `list_service_functions`
    // LISTS control functions but must never gain the power to run one, so any verb
    // that would execute/mutate is forbidden as a substring of every tool name.
    for forbidden in [
        "clear",
        "clear_faults",
        "clear_dtcs",
        "write",
        "code",
        "coding",
        "actuate",
        "io_control",
        "service_run",
        "run_service",
        "reset",
        "execute",
    ] {
        assert!(
            !tools.iter().any(|t| t.contains(forbidden)),
            "forbidden tool present: {forbidden}"
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
            variant: "d72n47a0".to_string(),
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
    let sgbd_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/Testmodule(1)/Ecu");
    let config =
        ServerConfig::parse_from(["klartext-mcp", "--sgbd-dir", sgbd_dir.to_str().unwrap()]);
    let server = KlartextServer::new(config);

    // Full catalog: 160 functions (156 control-table rows + 4 derived resets).
    let all = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: "d72n47a0".to_string(),
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
            variant: "d72n47a0".to_string(),
            risk: Some("low".to_string()),
        }))
        .await
        .unwrap();
    assert!(low.0.functions.iter().all(|f| f.risk == "low"));
    assert!(low.0.count < all.0.count);
}
