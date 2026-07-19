//! In-process integration tests driving the MCP tools directly (no real car).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;
use klartext_mcp::dto::{
    ClearAllFaultsRequest, ClearFaultsRequest, ConnectRequest, FaultHelpRequest,
    ListMeasurementsRequest, ListServiceFunctionIdsRequest, ListServiceFunctionsRequest,
    ReadAllFaultsRequest, ReadDataRequest, ReadFaultDetailRequest, ReadFaultsRequest,
    RunJobRequest, RunServiceFunctionRequest, ScanEcusRequest, StopServiceRequest,
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
         -- 0x18 is in the model map but absent on the mock car (not in the gateway SVT).
         INSERT INTO ecu VALUES (24,'egs_x','d_0018','Transmission',NULL);
         -- v4 extract: ISTA's measurement catalog (the \"index\"). Synthetic rows only.
         CREATE TABLE measurement (ecu_variant TEXT, name TEXT, unit TEXT, mul REAL, offset REAL, round INTEGER, zahlenformat TEXT, job TEXT);
         INSERT INTO measurement VALUES ('dde_x','STAT_EXAMPLE_TEMP_WERT','°C',1.0,0.0,0,NULL,'STATUS_LESEN');
         INSERT INTO measurement VALUES ('dde_x','STAT_EXAMPLE_RPM_WERT','1/min',1.0,0.0,0,NULL,'STATUS_MOTORDREHZAHL');",
    )
    .unwrap();
    (dir, path)
}

/// Build a synthetic semantic DB that also carries the M11 Item 4 repair-doc tables.
///
/// Extends the base fixture shape with a `fault_doc ⋈ infoobject` pair so `fault_help`
/// resolves offline. The DTC `0x4B1234` bridges to 4919860 (big-endian 24-bit concat)
/// and address 18 is `0x12`; two ISTA documents link to that fault.
fn fixture_db_with_docs() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE ecu (address INT, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
         INSERT INTO ecu VALUES (18,'dde_x','d_0012','Digital Diesel Electronics',NULL);
         CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
         INSERT INTO dtc VALUES (18,'dde_x',4919860,'P123400',NULL,'Glow plug circuit');
         CREATE TABLE fault_doc (address INT, code INT, infoobject_id INT, content_engb INT, content_dede INT);
         INSERT INTO fault_doc VALUES (18,4919860,1001,55501,55502);
         INSERT INTO fault_doc VALUES (18,4919860,1002,55601,55602);
         CREATE TABLE infoobject (id INT, infotype TEXT, docnumber TEXT, safety_relevant INT, title_en TEXT, title_de TEXT);
         INSERT INTO infoobject VALUES (1001,'FKB','DOC-1',0,'Glow plug fault','Gluehkerzenfehler');
         INSERT INTO infoobject VALUES (1002,'ABL','DOC-2',1,NULL,'Gluehkerze pruefen');",
    )
    .unwrap();
    (dir, path)
}

/// A synthetic semantic DB carrying a `job_param` service-function wiring.
///
/// Provides the ISTA-catalog metadata `run_service_function` reads — one fixed
/// function (`function_id` 1) whose Main phase runs the EDIABAS job
/// `STEUERN_E_LUEFTER_AUS`. That job is a REAL job in the BYO `d72n47a0.prg`: the
/// electric-fan release, which transmits a `0x2F` inputOutputControl (verified by
/// running it through `klartext_best` — it emits SID `0x2F` with an empty arg
/// buffer). No BMW data is embedded here; the `.prg` bytecode stays BYO.
fn fixture_db_with_service_function() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, rank INTEGER, position INTEGER, value TEXT, label TEXT, job TEXT);
         INSERT INTO job_param VALUES ('d72n47a0',1,'Electric fan release',NULL,'Main',1,1,'',NULL,'STEUERN_E_LUEFTER_AUS');",
    )
    .unwrap();
    (dir, path)
}

/// A synthetic semantic DB wiring a HELD service function (Main + Reset).
///
/// `function_id` 2 runs a Main job then a Reset job. Both are REAL jobs in the BYO
/// `d72n47a0.prg` with DISTINCT write SIDs, so the actuation and its teardown are
/// distinguishable on the wire: Main `STEUERN_E_LUEFTER_AUS` transmits `0x2F`
/// (inputOutputControl), Reset `STEUERN_LLKETA_RESET` transmits `0x31`
/// (routineControl). With NO `fixed_function` row, `hold_for(None, has_reset=true)`
/// resolves to `Hold::UntilStop`: after a successful Main the function HOLDS and its
/// teardown is deferred — the state the held→disconnect path exercises. The pairing
/// is chosen for real writes with distinct SIDs, not as a real ISTA hold-pair.
fn fixture_db_with_held_function() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, rank INTEGER, position INTEGER, value TEXT, label TEXT, job TEXT);
         INSERT INTO job_param VALUES ('d72n47a0',2,'Held actuation',NULL,'Main',1,1,'',NULL,'STEUERN_E_LUEFTER_AUS');
         INSERT INTO job_param VALUES ('d72n47a0',2,'Held actuation',NULL,'Reset',1,1,'',NULL,'STEUERN_LLKETA_RESET');",
    )
    .unwrap();
    (dir, path)
}

/// A synthetic DB with BOTH a HELD function (2, Main+Reset → `Hold::UntilStop`) and
/// a SECOND resolvable function (1, Main only). Used to prove the held-slot orphan
/// guard: with function 2 held, starting function 1 must be REFUSED rather than
/// overwriting the single held slot and stranding function 2's component.
fn fixture_db_with_two_functions() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, rank INTEGER, position INTEGER, value TEXT, label TEXT, job TEXT);
         INSERT INTO job_param VALUES ('d72n47a0',2,'Held actuation',NULL,'Main',1,1,'',NULL,'STEUERN_E_LUEFTER_AUS');
         INSERT INTO job_param VALUES ('d72n47a0',2,'Held actuation',NULL,'Reset',1,1,'',NULL,'STEUERN_LLKETA_RESET');
         INSERT INTO job_param VALUES ('d72n47a0',1,'Electric fan release',NULL,'Main',1,1,'',NULL,'STEUERN_E_LUEFTER_AUS');",
    )
    .unwrap();
    (dir, path)
}

/// A loopback gateway that positive-echoes writes so a held Main reaches `eoj`.
///
/// The shared [`spawn_mock_gateway`] stays SILENT to a write, which aborts the Main
/// job — and an aborted Main tears down at once (`held=false`), never holding. This
/// mock answers every non-keepalive request with a generic POSITIVE response
/// (`request SID | 0x40`, echoing the body), so a `STEUERN_*` job runs to completion
/// and the function actually HOLDS. Verified against the real `.prg`: the STEUERN
/// jobs used here reach `eoj` under this echo. Every non-keepalive payload is logged.
async fn spawn_mock_gateway_echoing_writes() -> (std::net::SocketAddr, FrameLog) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: FrameLog = std::sync::Arc::default();
    let shared = std::sync::Arc::clone(&log);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = std::sync::Arc::clone(&shared);
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    if frame.payload.as_slice() == [0x3E, 0x80] {
                        continue; // keepalive — unlogged, no reply
                    }
                    log.lock().unwrap().push((ecu, frame.payload.clone()));
                    let uds: Vec<u8> = match frame.payload.as_slice() {
                        [0x22, 0xF1, 0x90] => {
                            let mut u = vec![0x62, 0xF1, 0x90];
                            u.extend_from_slice(b"WBA3B5C50EK123456");
                            u
                        }
                        // Generic positive echo: request SID | 0x40, body unchanged.
                        // Enough for a STEUERN_* job to accept the response and reach
                        // eoj, so a held Main succeeds and the function holds.
                        [sid, rest @ ..] => {
                            let mut u = vec![sid | 0x40];
                            u.extend_from_slice(rest);
                            u
                        }
                        [] => continue,
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, uds); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    (addr, log)
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

// M11 Item 4: fault_help is a PURE semantic-DB read — no connect, no mock gateway.
// It resolves the ECU + DTC and returns the fault's linked ISTA documents (the
// title/pointer layer), degrading to empty docs (never erroring) without the extract.
#[tokio::test]
async fn fault_help_returns_linked_docs_offline() {
    // A server with a fixture semantic DB and NO car connection.
    let (_dir, db) = fixture_db_with_docs();
    let server = KlartextServer::new(config_with_db(&db));

    let out = server
        .fault_help(Parameters(FaultHelpRequest {
            ecu: "0x12".to_string(),
            code: "4B1234".to_string(),
        }))
        .await
        .unwrap();
    let r = out.0;
    assert_eq!(r.code_hex, "4B1234");
    // Two ISTA documents link to the fault; one is the English fault description.
    assert_eq!(r.docs.len(), 2);
    assert!(
        r.docs
            .iter()
            .any(|d| d.title.as_deref() == Some("Glow plug fault"))
    );
    // The German fallback title and the safety flag survive the DTO mapping.
    let procedure = r.docs.iter().find(|d| d.infoobject_id == 1002).unwrap();
    assert_eq!(procedure.title.as_deref(), Some("Gluehkerze pruefen"));
    assert!(procedure.safety_relevant);
    assert_eq!(procedure.docnumber.as_deref(), Some("DOC-2"));
    // DB-only: the fault text also resolved from the dtc table.
    assert!(
        r.descriptions
            .iter()
            .any(|d| d.text.as_deref() == Some("Glow plug circuit"))
    );
    // Non-empty-docs note is the title-layer caveat, not a "build the DB" message.
    assert!(r.note.contains("ISTA document"), "{}", r.note);
    // This fixture has no sibling klartext-docs.db, so the rendered FKB body degrades
    // to empty (the `docs` pointers still apply). The positive render path is unit-tested
    // in klartext-semantic's fault_body_reads_rendered_markdown_from_sibling_docs_db.
    assert!(r.body.is_empty());
}

/// The recorded, ordered `(target ECU, UDS payload)` pairs a mock gateway has
/// received (keepalives excluded). The address is kept alongside the payload so a
/// test can tell which ECU a frame was sent to — e.g. that a reset never reached
/// the gateway — even though every mock ECU shares one demultiplexed connection
/// and log. Most tests only care about the payload sequence; see
/// [`payloads_only`] for that shorter, address-blind view.
type FrameLog = std::sync::Arc<std::sync::Mutex<Vec<(u8, Vec<u8>)>>>;

/// Project a captured [`FrameLog`] snapshot down to its payloads, in order.
///
/// The address-blind view most tests want: an exact/contains assertion over the
/// UDS bytes a target-agnostic path (e.g. one ECU's own read/clear sequence) sent.
fn payloads_only(frames: &[(u8, Vec<u8>)]) -> Vec<Vec<u8>> {
    frames.iter().map(|(_, payload)| payload.clone()).collect()
}

/// The ECUs the mock car answers for; 0x18 (in the DB map) is deliberately absent.
const MOCK_PRESENT: &[u8] = &[0x10, 0x12, 0x40];

/// A loopback mock gateway with several ECUs demultiplexed over one connection.
///
/// Answers the SVT installed-ECU list (`22 3F 07`, used by scan discovery),
/// VIN/DTC/PID reads, the dynamic-measurement sequence, and the extended-session +
/// standard-clear handshakes; ignores keepalives. Only [`MOCK_PRESENT`] addresses
/// answer, and the SVT it returns lists exactly that set. Every reply swaps SRC/TGT
/// (as the real gateway does), so the client's demux routes it by the answering
/// ECU's address. Each ECU tracks a "cleared" flag so a post-clear re-read comes
/// back clean. Every non-keepalive UDS payload is recorded in the returned log for
/// exact-frame assertions.
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
                    // ISTA's functional (broadcast) clear. Logged under its own 0xDF
                    // target so a test can assert it went out FIRST, then answered by
                    // each responder from its OWN source address — the shape a real
                    // broadcast has. 0x10 deliberately stays silent so the straggler
                    // pass has something to do.
                    if ecu == klartext_uds::FUNCTIONAL_ADDRESS_F01 {
                        log.lock().unwrap().push((ecu, frame.payload.clone()));
                        if frame.payload.as_slice() == [0x14, 0xFF, 0xFF, 0xFF] {
                            for responder in [0x12u8, 0x40] {
                                cleared.insert(responder);
                                let reply = HsfzFrame::diagnostic(responder, tester, vec![0x54]);
                                let _ = write_frame(&mut stream, &reply).await;
                            }
                        }
                        continue;
                    }
                    if !MOCK_PRESENT.contains(&ecu) {
                        continue; // absent ECU — silence (a read there times out)
                    }
                    log.lock().unwrap().push((ecu, frame.payload.clone()));
                    let reply = match frame.payload.as_slice() {
                        [0x3E, 0x00] => vec![0x7E, 0x00], // presence probe
                        [0x22, 0xF1, 0x90] => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(b"WBA3B5C50EK123456");
                            uds
                        }
                        // SVT installed-ECU list (22 3F 07) from the ZGW: a u16-BE count
                        // then one address byte per fitted ECU. Returns MOCK_PRESENT so
                        // SVT discovery yields exactly the ECUs that answer reads (0x18,
                        // in the DB map, is not listed). DERIVED framing
                        // (STATUS_VCM_GET_ECU_LIST_ALL), [verify against capture].
                        [0x22, 0x3F, 0x07] => {
                            let mut uds = vec![0x62, 0x3F, 0x07, 0x00, MOCK_PRESENT.len() as u8];
                            uds.extend_from_slice(MOCK_PRESENT);
                            uds
                        }
                        // The actively-responding subset (22 3F 08,
                        // STATUS_VCM_GET_ECU_LIST_ACTIVE_RESPONSE). This mock does not
                        // model a "responding" set distinct from the fitted one, so it
                        // answers negatively (requestOutOfRange) — exactly like a real
                        // gateway that lacks the DID — rather than staying silent: a
                        // silent reply here would cost scan_ecus/read_responding_ecu_list
                        // a full read-timeout wait for no reason (both already treat a
                        // rejection as `None`, same as an absent DID).
                        [0x22, 0x3F, 0x08] => vec![0x7F, 0x22, 0x31],
                        // The gateway identity reads (M11 Item 2): integration level
                        // (I-Stufe, 22 100B) and the vehicle order (FA, 22 3F06, raw
                        // region). I-Stufe is a binary-packed 8-byte record ("F020" +
                        // year 21 (0x15) + month 11 (0x0B) + patch 500 (0x01F4)) →
                        // "F020-21-11-500"; the FA framing stays [verify against capture].
                        [0x22, 0x10, 0x0B] => {
                            let mut uds = vec![0x62, 0x10, 0x0B];
                            uds.extend_from_slice(&[
                                0x46, 0x30, 0x32, 0x30, 0x15, 0x0B, 0x01, 0xF4,
                            ]);
                            uds
                        }
                        [0x22, 0x3F, 0x06] => vec![0x62, 0x3F, 0x06, 0xAA, 0xBB],
                        // Any other identification DID (22 F1xx besides the VIN F190
                        // handled above) is answered negatively so identify_vehicle's
                        // per-ECU identification skips it fast — exactly as a real ECU
                        // that does not serve the DID would. requestOutOfRange (0x31).
                        [0x22, 0xF1, _] => vec![0x7F, 0x22, 0x31],
                        // OBDDataIdentifier for PID 0x0C (engine RPM): 0D 48 -> 850 rpm.
                        [0x22, 0xF4, 0x0C] => vec![0x62, 0xF4, 0x0C, 0x0D, 0x48],
                        // The DDE's static `0x22` read of DID 0x4517 (SG_FUNKTIONEN row
                        // ITOEL) — the frame the real STATUS_LESEN(ARG;ITOEL) bytecode
                        // emits (frozen in crates/best/tests/differential.rs). Answers a
                        // raw word the job scales; drives run_job's live read path.
                        [0x22, 0x45, 0x17] => vec![0x62, 0x45, 0x17, 0x0A, 0xBC],
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
                        // Freeze-frame detail (M11) for DTC 24 00 00. Snapshot: record
                        // 1, 2 identifiers — coolant 0x5205 = 0x7B, RPM 0x5955 = 0x1068.
                        // Extended: HFK (0x02) = 0x1F. Severity: 0x20 / 0x10. DERIVED
                        // ISO 14229 framing, [verify against capture].
                        [0x19, 0x04, 0x24, 0x00, 0x00, 0xFF] => vec![
                            0x59, 0x04, 0x24, 0x00, 0x00, 0x08, 0x01, 0x02, 0x52, 0x05, 0x7B, 0x59,
                            0x55, 0x10, 0x68,
                        ],
                        [0x19, 0x06, 0x24, 0x00, 0x00, 0xFF] => {
                            vec![0x59, 0x06, 0x24, 0x00, 0x00, 0x08, 0x02, 0x1F]
                        }
                        [0x19, 0x09, 0x24, 0x00, 0x00] => {
                            vec![0x59, 0x09, 0xFF, 0x20, 0x10, 0x24, 0x00, 0x00, 0x08]
                        }
                        // After a clear, this ECU reads clean.
                        // Only the ISTA mask (`19 02 0C`) is served — a regression to
                        // `19 02 FF` falls through to the catch-all and times out
                        // rather than quietly receiving the same answer.
                        [0x19, 0x02, 0x0C] if cleared.contains(&ecu) => vec![0x59, 0x02, 0x0C],
                        // Two DTCs with DIFFERENT presence verdicts under ISTA's rule:
                        // D9040A status 0x08 -> Absent (stored, bit 0 clear), and
                        // AABBCC status 0x2F -> Present (bit 0 set, bit 6 clear).
                        [0x19, 0x02, 0x0C] => vec![
                            0x59, 0x02, 0x0C, 0xD9, 0x04, 0x0A, 0x08, 0xAA, 0xBB, 0xCC, 0x2F,
                        ],
                        // The info memory (22 2000, IS_LESEN) that scan_faults now reads
                        // alongside fault memory. These mock ECUs keep none, so it answers
                        // a clean negative (requestOutOfRange) and the fault+info bundle
                        // degrades to info_supported=false — never a timeout that would
                        // wrongly mark the ECU errored.
                        [0x22, 0x20, 0x00] => vec![0x7F, 0x22, 0x31],
                        // Extended session + the standard clear-all (M9 Part B).
                        [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
                        [0x14, 0xFF, 0xFF, 0xFF] => {
                            cleared.insert(ecu);
                            vec![0x54]
                        }
                        // ECU reset: a real POSITIVE response, deliberately kept
                        // after the post-clear reset was removed (parity audit
                        // P0.1). The no-reset assertions are only meaningful if a
                        // reintroduced `11 01` would succeed and be captured here
                        // rather than silently time out.
                        [0x11, 0x01] => vec![0x51, 0x01],
                        // The gateway's combined fault store (ZFS). Note the value
                        // byte is 00, not the FF the `zgw_01.prg` template shows —
                        // the job overwrites that placeholder before transmitting.
                        [0x31, 0x01, 0x40, 0x00, 0x00] if ecu == 0x10 => {
                            vec![0x71, 0x01, 0x40, 0x00]
                        }
                        // The terminal-15 payloads are LOGGED BUT NEVER ANSWERED, on
                        // purpose. Answering them would make the server wait out the
                        // real 15-second clamp hold and add 15 s to this suite. The
                        // cycle's behaviour is proven where it belongs — in
                        // `crates/client`, against the `cycle_terminal_15_holding`
                        // seam with a 1 ms hold. What this mock proves is that the
                        // server still TRANSMITS the step: the frame lands in the log.
                        // Do not add an arm for it here.
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

/// As [`config_for_mock`], with a short read timeout — which is also the quiet period
/// a functional broadcast collects for, and the deadline a deliberately-unanswered
/// frame waits out. Used by the whole-car clear, which does both.
fn config_for_mock_fast(addr: std::net::SocketAddr, db: &Path) -> ServerConfig {
    ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--timeout",
        "150",
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
        }))
        .await
        .unwrap();
    assert_eq!(result.0.address, "0x40");
    // P0.2: every fault the ECU returned is shown — klartext no longer filters by
    // status. The mock serves two records; both must appear.
    assert_eq!(result.0.count, 2);
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
async fn read_fault_detail_reads_all_three_services_and_degrades_without_sgbd() {
    // Without --sgbd-dir the fields cannot be decoded, but the plumbing still runs:
    // all three reads succeed, severity is parsed, and the notes explain the raw
    // state and the capture caveat. Runs in CI (no BYO data needed).
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_fault_detail(Parameters(ReadFaultDetailRequest {
            ecu: "0x12".to_string(),
            code: "240000".to_string(),
            variant: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.code_hex, "240000");
    // Severity (19 09) is parsed even without the SGBD.
    assert_eq!(result.0.severity_hex.as_deref(), Some("20"));
    assert_eq!(result.0.functional_unit_hex.as_deref(), Some("10"));
    // No SGBD → fields stay raw, flagged, with the derived-framing caveat.
    assert!(!result.0.sgbd_available);
    assert!(result.0.snapshot.is_empty());
    assert!(
        result.0.notes.iter().any(|n| n.contains("no SGBD variant")),
        "expected a no-SGBD note, got {:?}",
        result.0.notes
    );
    assert!(
        result.0.notes.iter().any(|n| n.contains("provisional")),
        "expected the capture caveat note"
    );
}

#[tokio::test]
async fn read_fault_detail_rejects_a_malformed_code() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    let result = server
        .read_fault_detail(Parameters(ReadFaultDetailRequest {
            ecu: "0x12".to_string(),
            code: "24ZZ".to_string(),
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an invalid-code error, got Ok");
    };
    assert!(err.message.contains("invalid DTC code"), "{}", err.message);
}

// The full decode path with the real DDE SGBD: the snapshot's coolant (0x5205, u8
// − 40) and RPM (0x5955, u16 × 0.5) fields resolve to values. Ignored by default
// (needs the BYO `.prg`). The wire bytes are DERIVED, not captured.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn read_fault_detail_decodes_snapshot_with_real_sgbd() {
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
        .read_fault_detail(Parameters(ReadFaultDetailRequest {
            ecu: "0x12".to_string(),
            code: "240000".to_string(),
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    assert!(result.0.sgbd_available);
    // Coolant 0x5205 = 0x7B (123 − 40 = 83 °C) and RPM 0x5955 = 0x1068 (4200 × 0.5).
    let coolant = result
        .0
        .snapshot
        .iter()
        .find(|f| f.id_hex == "5205")
        .expect("coolant field decoded");
    assert!((coolant.value.unwrap() - 83.0).abs() < 0.01);
    let rpm = result
        .0
        .snapshot
        .iter()
        .find(|f| f.id_hex == "5955")
        .expect("rpm field decoded");
    assert!((rpm.value.unwrap() - 2100.0).abs() < 0.01);
    // Extended data: the HFK occurrence counter.
    assert!(result.0.extended.iter().any(|r| r.label == "HFK"));
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
//
// This is ALSO the regression guard for parity audit P0.1 — klartext must send NO
// UDS 0x11 ECUReset after a clear, because ISTA's own whole-vehicle clear
// (`VehicleIdent.ClearErrorInfoMemoryVehicle`, VehicleIdent.cs:9720-9788) sends none.
// klartext DID send one until 2026-07-18. The mock answers `11 01` with a genuine
// `51 01` (see spawn_mock_gateway), so a reintroduced reset would SUCCEED and appear
// in this census rather than silently time out — the assertion below is exact, not a
// "contains", so an extra frame anywhere fails it.
#[tokio::test]
async fn clear_faults_sends_only_the_standard_frames_and_no_ecu_reset() {
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
    assert!(result.0.note.contains("not reset"), "{}", result.0.note);

    let frames = payloads_only(&frames.lock().unwrap());
    assert_eq!(
        frames,
        vec![
            vec![0x22, 0xF1, 0x90], // connect: VIN read (from the gateway)
            vec![0x19, 0x02, 0x0C], // pre-read: record what will be discarded
            vec![0x10, 0x03],       // extended session (required before a clear)
            vec![0x14, 0xFF, 0xFF, 0xFF], // standard clear-all (M2 path, no new frame)
                                    // ...and NOTHING after it: no 0x11 reset.
        ]
    );
}

// Item 5 P2 fix (2026-07-10): STATUS_LESEN is a static-only reader — it emits a
// static `0x22 <id>` that a real ECU REJECTS (`7F 22 31`) for a DYNAMIC (2C-define)
// measurement (car-session-1 finding 1). `run_job` now refuses such a call and
// redirects the caller to `read_data`, which drives the selektiv-lesen sequence,
// instead of transmitting the doomed static read
// (`klartext_semantic::misrouted_dynamic_measurement`, checked in `mcp/src/server.rs`
// `run_job` before the read-only gate is even built). ITOEL (oil temperature, id
// 0x4517) is dynamic on the real DDE SGBD (`SERVICE="22;2C"`); this pins the refusal
// at the MCP tool boundary — `misrouted_dynamic_measurement` itself is unit-tested in
// `crates/semantic/src/measurement.rs`. This REPLACES the stale
// `run_job_reads_named_results_over_the_read_only_gate`, which asserted the
// pre-fix behaviour (the job succeeding on ITOEL) and has failed ever since the fix
// shipped. Ignored by default (BYO `.prg`); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn run_job_redirects_a_dynamic_measurement_to_read_data() {
    let (addr, frames) = spawn_mock_gateway().await;
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
        .run_job(Parameters(RunJobRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            job: "STATUS_LESEN".to_string(),
            args: vec!["ARG".to_string(), "ITOEL".to_string()],
        }))
        .await;

    let Err(err) = result else {
        panic!("expected the dynamic-measurement redirect to refuse the job");
    };
    assert!(err.message.contains("read_data"), "{}", err.message);
    assert!(err.message.contains("dynamic"), "{}", err.message);
    assert!(err.message.contains("STATUS_LESEN"), "{}", err.message);

    // Refused before the wire: the doomed static read the un-redirected job would
    // have sent (0x22 4517, per crates/best/tests/differential.rs) never reached
    // the mock — the redirect fires before the read-only gate is even built.
    let frames = payloads_only(&frames.lock().unwrap());
    assert!(
        !frames.iter().any(|f| f.as_slice() == [0x22, 0x45, 0x17]),
        "the redirect should refuse before transmitting: {frames:02X?}"
    );
}

// No committed test exercises `run_job` succeeding on a STATIC measurement (the
// redirect's negative case) against the DDE SGBD: a throwaway probe (not committed)
// loading `d72n47a0.prg` through `klartext_semantic::Measurements::from_sgbd` found
// that all 1,787 `SG_FUNKTIONEN` rows carry `SERVICE="22;2C"` — every proprietary
// measurement on this ECU is dynamic, so `STATUS_LESEN` never has a legitimate
// direct target here (the 3-row sample in `crates/best/tests/differential.rs`
// turned out to generalize to the whole table, not just those 3 rows). A static row
// does exist elsewhere — the DSC `dsc_10.prg` id `0x4005`
// (`crates/best/tests/differential.rs`,
// `vm_status_lesen_decodes_a_multi_row_res_table_on_the_dsc`) — but that ECU
// (address 0x29) is not in this file's `MOCK_PRESENT`, and wiring in a second mock
// ECU for one test is a bigger change than this fix warrants. If a future SGBD
// extraction adds a static-measurement row to the DDE, add the positive-case test
// here.

// Item 5 P2 — the BEHAVIORAL half of the read-only invariant (the surface test
// `advertises_exactly_the_refined_tool_surface` is the structural half). `run_job`
// runs an ECU's own bytecode, which could in principle emit ANY UDS service; the
// single thing that keeps it read-only is the `GatedExchange::read_only` its
// transport is wrapped in (server.rs `run_job`). Here we drive that EXACT gate
// composition — `GatedExchange::read_only(TelegramExchange::new(<bridge over a real
// client>))` — against the suite's frame-recording mock, feeding it a WRITE (0x2E)
// telegram of the shape a `STEUERN_*` job's `xsend` would build. The gate must
// refuse it at the transmit boundary, so NO write frame reaches the wire; a READ
// (0x22) through the same stack then DOES reach the mock, proving the write's
// absence is the gate refusing — not a severed transport.
//
// Why not drive the whole `run_job` TOOL with a write-emitting job? That needs a BYO
// `.prg` whose bytecode emits a write (BMW data, uncommittable), so the tool-level
// write case cannot be a committed test — the read-path tool test
// `run_job_redirects_a_dynamic_measurement_to_read_data` is `#[ignore]` for the same
// reason. This proves the same seam `run_job` relies on, over the real client + HSFZ
// transport, with no BYO data. (The gate's own veto — write refused, inner never
// touched — is unit-tested in `crates/best/src/gate.rs`; the Refused→invalid_request
// mapping in `mcp/src/server.rs`. The `ClientBridge` below is byte-identical to the
// crate-private `SessionBridge` `run_job` uses.)
#[tokio::test]
async fn run_job_gate_refuses_a_write_before_the_wire() {
    use klartext_best::{
        BareUdsTransport, ExchangeError, GatedExchange, TelegramExchange, UdsExchange, encode,
    };
    use klartext_client::{ClientConfig, DiagnosticClient};

    // A bare-UDS bridge onto the live client — identical to the server's private
    // `SessionBridge`, reproduced here only because that type is crate-internal.
    struct ClientBridge<'a> {
        client: &'a DiagnosticClient,
    }
    #[async_trait::async_trait]
    impl BareUdsTransport for ClientBridge<'_> {
        async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            self.client
                .request(target, uds)
                .await
                .map_err(|e| ExchangeError::Transport(format!("{e}")))
        }
    }

    let (addr, frames) = spawn_mock_gateway().await;
    let client = DiagnosticClient::connect(
        addr.ip(),
        &ClientConfig {
            port: addr.port(),
            ..Default::default()
        },
    )
    .await
    .expect("connect to mock gateway");

    // The EXACT stack `run_job` wraps its transport in (server.rs `run_job`).
    let gate = GatedExchange::read_only(TelegramExchange::new(ClientBridge { client: &client }));

    // A write telegram of the shape a STEUERN_* job's `xsend` transmits: 0x2E
    // writeDataByIdentifier to ECU 0x12. The gate must refuse it at the seam.
    let write = encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);
    match gate.request(0x12, &write).await {
        Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x2E),
        other => panic!("expected the read-only gate to refuse the write, got {other:?}"),
    }
    // No write/actuation/flashing frame reached the car — the gate refused before the
    // transport was ever touched.
    let after_write = payloads_only(&frames.lock().unwrap());
    assert!(
        !after_write.iter().any(|f| matches!(
            f.first(),
            Some(0x2E | 0x31 | 0x2F | 0x14 | 0x27 | 0x34..=0x37)
        )),
        "a write frame reached the car: {after_write:02X?}"
    );

    // Positive control: a READ (0x22 VIN) through the SAME gate DOES reach the mock,
    // proving the transport is live — so the write's absence above is the gate
    // refusing, not a dead link.
    gate.request(0x12, &encode(0x12, 0xF1, &[0x22, 0xF1, 0x90]))
        .await
        .expect("the read-only gate must pass a read to the car");
    let after_read = payloads_only(&frames.lock().unwrap());
    assert!(
        after_read
            .iter()
            .any(|f| f.as_slice() == [0x22, 0xF1, 0x90]),
        "the read never reached the car: {after_read:02X?}"
    );
}

// The P3 surface invariant: read tools (including the read-only EDIABAS job runner
// `run_job`) plus a bounded, confirm-gated WRITE surface — the two clears
// (clear_faults / clear_all_faults, UDS 0x14) and the two service-write tools
// (run_service_function / stop_service, which run an ECU service function through the
// BEST/2 VM under the confirmed-write gate). Every write/actuation tool refuses
// without confirm=true; flashing (0x34–0x37) is never a tool. This test is the
// STRUCTURAL guard: the surface is EXACTLY these tools, so a NEW, un-reviewed write
// verb appearing as a tool fails the exact-list assertion below.
//
// The wire-level guarantees live in dedicated tests:
// `clear_faults_sends_only_the_standard_frames_and_no_ecu_reset` (an EXACT frame
// census), `run_job_gate_refuses_a_write_before_the_wire` (the read-only seam blocks
// a write), and `service_write_passes_confirmed_gate_but_read_only_refuses` (the
// confirmed-write seam ADMITS a write that the read-only seam blocks). The confirm
// gate on both service tools is pinned by their refusal tests.
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
            "fault_help".to_string(),
            "identify_vehicle".to_string(),
            "list_ecus".to_string(),
            "list_measurements".to_string(),
            "list_service_function_ids".to_string(),
            "list_service_functions".to_string(),
            "read_all_faults".to_string(),
            "read_data".to_string(),
            "read_fault_detail".to_string(),
            "read_faults".to_string(),
            "read_info_memory".to_string(),
            "run_job".to_string(),
            "run_service_function".to_string(),
            "scan_ecus".to_string(),
            "stop_service".to_string(),
        ]
    );
    // The write surface is named by ROLE — run_service_function / stop_service /
    // clear_faults / clear_all_faults — never by a bare actuation/coding verb. These
    // verbs stay forbidden as a substring of ANY tool name, so a NEW tool leaking one
    // (e.g. `actuate_component`, `write_coding`, `io_control`) would trip here and
    // force review. The two service-write tools deliberately avoid them; their
    // actuation is gated by confirm=true (their refusal tests) and by the
    // confirmed-write transmit seam, not by their name.
    for forbidden in [
        "actuat",
        "io_control",
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
    // The `run`-named tools are the read-only job runner and the confirm-gated
    // service-function runner; both are accounted for here, so a future `run_*` tool
    // (e.g. `run_actuator`) would still trip this dedicated check.
    let run_tools: Vec<&str> = tools
        .iter()
        .filter(|t| t.contains("run"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        run_tools,
        vec!["run_job", "run_service_function"],
        "only run_job and run_service_function may contain 'run'"
    );
    // "clear" appears only on the two confirmation-gated clears (per-ECU + whole-car),
    // both standard UDS 0x14 — never on an actuation/coding verb.
    let mut clears: Vec<&String> = tools.iter().filter(|t| t.contains("clear")).collect();
    clears.sort();
    assert_eq!(clears, vec!["clear_all_faults", "clear_faults"]);
}

// ── run_service_function / stop_service (P3 Task 7) ───────────────────────────

#[tokio::test]
async fn run_service_function_refuses_without_confirm() {
    // The confirmation gate is checked before anything else — even before the "not
    // connected" check — so a refusal never touches the car. With the invented
    // precondition gate removed (owner ruling 3), this is the PRIMARY safety
    // mechanism: the tool must not actuate a component without the human's go-ahead.
    let server = KlartextServer::new(test_config());
    let result = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: None,
            function_id: 9001,
            confirm: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a refusal without confirm, got Ok");
    };
    // Names the function and the actuation risk, and never reaches the connection.
    assert!(err.message.contains("9001"), "{}", err.message);
    assert!(err.message.contains("ACTUATES"), "{}", err.message);
    assert!(err.message.contains("confirm=true"), "{}", err.message);
    assert!(!err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn stop_service_refuses_without_confirm() {
    // The stop path is also confirm-gated: its teardown moves the component, so it
    // refuses before touching the car, exactly like run_service_function.
    let server = KlartextServer::new(test_config());
    let result = server
        .stop_service(Parameters(StopServiceRequest {
            ecu: "0x12".to_string(),
            variant: None,
            function_id: 9001,
            confirm: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a refusal without confirm, got Ok");
    };
    assert!(err.message.contains("9001"), "{}", err.message);
    assert!(err.message.contains("confirm=true"), "{}", err.message);
    assert!(!err.message.contains("not connected"), "{}", err.message);
}

// P3 Task 7 — the wire proof for the SERVICE-WRITE seam (the confirm-refusal tests
// above are the human-confirmation half). run_service_function runs each job through
// `ConfirmedWriteBridge`, which composes
// `GatedExchange::confirmed_write(TelegramExchange::new(<bridge over the live
// client>))` — byte-identical to what run_job builds, but with the confirmed-write
// policy. Here we drive that EXACT composition over the suite's frame-recording mock:
// a WRITE (0x2E) telegram, of the shape a STEUERN_* job's `xsend` builds, is ADMITTED
// and reaches the wire under confirmed_write, while the SAME telegram through
// read_only is REFUSED at the seam and never reaches the car — and a flashing SID
// (0x34) stays refused even under confirmed_write, so the ladder is not a blanket
// allow-all. This is the committed, no-BYO-data proof; a tool-level run needs a
// `.prg` whose bytecode emits a write (BMW data, uncommittable), exactly as
// `run_job_gate_refuses_a_write_before_the_wire` is for the read path. The gate's
// per-SID classification is unit-tested in crates/best/src/gate.rs; this proves the
// confirmed-write COMPOSITION on the wire.
#[tokio::test]
async fn service_write_passes_confirmed_gate_but_read_only_refuses() {
    use klartext_best::{
        BareUdsTransport, ExchangeError, GatedExchange, TelegramExchange, UdsExchange, encode,
    };
    use klartext_client::{ClientConfig, DiagnosticClient};

    // The live-client bridge — identical to the server's crate-private SessionBridge
    // that ConfirmedWriteBridge wraps, reproduced here because it is internal.
    struct ClientBridge<'a> {
        client: &'a DiagnosticClient,
    }
    #[async_trait::async_trait]
    impl BareUdsTransport for ClientBridge<'_> {
        async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            self.client
                .request(target, uds)
                .await
                .map_err(|e| ExchangeError::Transport(format!("{e}")))
        }
    }

    let (addr, frames) = spawn_mock_gateway().await;
    // A short read timeout: the mock does not answer a 0x2E write, so the ADMITTED
    // write below returns a transport timeout once its frame has left the tester —
    // the frame reaching the wire is the proof, not a reply.
    let client = DiagnosticClient::connect(
        addr.ip(),
        &ClientConfig {
            port: addr.port(),
            read_timeout: Duration::from_millis(150),
            ..Default::default()
        },
    )
    .await
    .expect("connect to mock gateway");

    // A write telegram of the shape a STEUERN_* job's `xsend` transmits: 0x2E
    // writeDataByIdentifier to ECU 0x12.
    let write = encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);

    // read_only REFUSES the write at the seam — nothing reaches the car.
    let read_gate =
        GatedExchange::read_only(TelegramExchange::new(ClientBridge { client: &client }));
    match read_gate.request(0x12, &write).await {
        Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x2E),
        other => panic!("read_only must refuse the write, got {other:?}"),
    }
    assert!(
        !payloads_only(&frames.lock().unwrap())
            .iter()
            .any(|f| f.first() == Some(&0x2E)),
        "read_only let a write reach the car"
    );

    // confirmed_write ADMITS the write: it reaches the wire (the gate did not refuse).
    let write_gate =
        GatedExchange::confirmed_write(TelegramExchange::new(ClientBridge { client: &client }));
    let admitted = write_gate.request(0x12, &write).await;
    assert!(
        !matches!(admitted, Err(ExchangeError::Refused { .. })),
        "confirmed_write must ADMIT the write, not refuse it: {admitted:?}"
    );
    assert!(
        payloads_only(&frames.lock().unwrap())
            .iter()
            .any(|f| f.as_slice() == [0x2E, 0x10, 0x01, 0xFF]),
        "the confirmed write never reached the car: {:02X?}",
        payloads_only(&frames.lock().unwrap())
    );

    // Flashing stays refused even under confirmed_write — the ladder is not a blanket
    // allow-all. 0x34 requestDownload must never leave the tester.
    let flash = encode(0x12, 0xF1, &[0x34, 0x00]);
    match write_gate.request(0x12, &flash).await {
        Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x34),
        other => panic!("confirmed_write must still refuse flashing, got {other:?}"),
    }
    assert!(
        !payloads_only(&frames.lock().unwrap())
            .iter()
            .any(|f| f.first() == Some(&0x34)),
        "a flashing frame reached the car under confirmed_write"
    );
}

// P3 Task 7 (review gap): the END-TO-END proof that run_service_function's own
// ConfirmedWriteBridge uses confirmed_write, NOT read_only. The composition test
// above proves the gate classifies, but builds its own gate — flipping
// ConfirmedWriteBridge's policy in server.rs would not fail it. This drives the REAL
// tool: run_service_function runs the electric-fan release (Main job
// STEUERN_E_LUEFTER_AUS, a real job in the BYO d72n47a0.prg that transmits a 0x2F
// inputOutputControl) through the VM under the bridge, and asserts the 0x2F write
// reached the mock gateway — which only happens if the bridge ADMITTED it. Flip
// ConfirmedWriteBridge to read_only and the write is refused at the seam and never
// reaches the wire, so this test fails (verified by mutation). Ignored by default
// (needs the BYO `.prg`); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn run_service_function_transmits_a_write_over_the_confirmed_write_bridge() {
    let (addr, frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db_with_service_function();
    let sgbd_dir = sgbd_test_dir();
    // Short --timeout: the mock does not answer the 0x2F write, so the VM's exchange
    // times out once the frame has left the tester — the frame reaching the wire is
    // the proof, not a reply (the same trick the composition test uses).
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
        "--timeout",
        "150",
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // The job aborts once its 0x2F goes unanswered, so the tool reports a failed
    // cycle — but the frame has already left the tester, which is the proof.
    let _ = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            function_id: 1,
            confirm: true,
        }))
        .await;

    let payloads = payloads_only(&frames.lock().unwrap());
    assert!(
        payloads.iter().any(|f| f.first() == Some(&0x2F)),
        "the service-function write (0x2F) never reached the car — the confirmed-write \
         bridge did not admit it: {payloads:02X?}"
    );
}

// P3 Task 7 (review gap 2): the held→disconnect→teardown behavioral proof. A held
// actuation (Activation==0 with a Reset phase) actuates and DEFERS its teardown; that
// deferred Reset MUST run on disconnect (owner ruling 2) so a component is never left
// forced past the session. This drives the real tools end to end: run_service_function
// on function 2 succeeds its Main (STEUERN_E_LUEFTER_AUS, one 0x2F) against a
// positive-echoing mock and reports held=true with teardown "deferred" — and the Reset
// job (STEUERN_LLKETA_RESET, one 0x31) has NOT gone out yet. disconnect() then runs it,
// so the 0x31 teardown frame reaches the wire only AFTER the disconnect. Verified by
// mutation (reverting disconnect to a plain session-drop leaves 0x31 off the wire).
// Ignored by default (needs the BYO `.prg`); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn held_service_function_is_torn_down_on_disconnect_over_the_wire() {
    let (addr, frames) = spawn_mock_gateway_echoing_writes().await;
    let (_dir, db) = fixture_db_with_held_function();
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
        "--timeout",
        "150",
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // Run the held function: the echo lets Main reach eoj, so it SUCCEEDS and the
    // function HOLDS — its teardown is deferred, not run.
    let run = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            function_id: 2,
            confirm: true,
        }))
        .await
        .unwrap();
    assert!(
        run.0.held,
        "an Activation==0 function whose Main succeeded must report it is holding"
    );
    assert_eq!(
        run.0.teardown, "deferred",
        "the teardown must be deferred, not run"
    );

    // The Main actuation (0x2F) is on the wire; the Reset teardown (0x31) is NOT yet.
    let before = payloads_only(&frames.lock().unwrap());
    assert!(
        before.iter().any(|f| f.first() == Some(&0x2F)),
        "the Main actuation (0x2F) never reached the car: {before:02X?}"
    );
    assert!(
        !before.iter().any(|f| f.first() == Some(&0x31)),
        "the Reset teardown (0x31) ran before disconnect — it must be deferred: {before:02X?}"
    );

    // Disconnect must run the deferred teardown (owner ruling 2) against the still-open
    // session before dropping it — the 0x31 Reset frame reaches the wire only now.
    let disc = server.disconnect().await.unwrap();
    assert!(disc.0.was_connected);
    let after = payloads_only(&frames.lock().unwrap());
    assert!(
        after.iter().any(|f| f.first() == Some(&0x31)),
        "disconnect did not run the held function's teardown (0x31) on the wire: {after:02X?}"
    );
}

// The held-slot ORPHAN guard, end to end. klartext tracks ONE held actuation, so
// starting a SECOND held function while the first is outstanding would overwrite the
// slot and strand the first component energised with no teardown path. The guard
// refuses instead. Without it, the second run would proceed and orphan the first.
// Ignored by default (needs the BYO `.prg`); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn run_service_function_refuses_while_a_different_function_is_held() {
    let (addr, _frames) = spawn_mock_gateway_echoing_writes().await;
    let (_dir, db) = fixture_db_with_two_functions();
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
        "--timeout",
        "150",
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // Function 2 holds (Main succeeds via the echo, teardown deferred).
    let held = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            function_id: 2,
            confirm: true,
        }))
        .await
        .unwrap();
    assert!(
        held.0.held,
        "function 2 must be holding for this test to mean anything"
    );

    // Now a DIFFERENT function (1) must be REFUSED — starting it would orphan 2.
    let Err(err) = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            function_id: 1,
            confirm: true,
        }))
        .await
    else {
        panic!("a second held actuation must be refused, not run");
    };
    // The refusal names the OUTSTANDING function (2) and points at the stop, so the
    // agent knows exactly what to tear down first.
    assert!(err.message.contains("HELD"), "{}", err.message);
    assert!(
        err.message.contains(" 2 "),
        "must name the held function 2: {}",
        err.message
    );
    assert!(err.message.contains("stop_service"), "{}", err.message);

    // Re-running the SAME held function (2) is allowed — it targets the same
    // component, so it cannot strand a second one.
    let same = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            function_id: 2,
            confirm: true,
        }))
        .await;
    assert!(
        same.is_ok(),
        "re-running the same held function must not be refused: {:?}",
        same.err().map(|e| e.message)
    );
}

// A RECONNECT (a second `connect`) must tear down an outstanding held actuation
// against the still-live old session, not just drop it and rely on the ECU's S3
// revert (review finding 1b). Same mechanism as disconnect, at a different door.
// Ignored by default (needs the BYO `.prg`); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn a_reconnect_tears_down_a_held_actuation_over_the_wire() {
    let (addr, frames) = spawn_mock_gateway_echoing_writes().await;
    let (_dir, db) = fixture_db_with_held_function();
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
        "--timeout",
        "150",
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    let held = server
        .run_service_function(Parameters(RunServiceFunctionRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            function_id: 2,
            confirm: true,
        }))
        .await
        .unwrap();
    assert!(held.0.held, "function 2 must be holding");
    assert!(
        !payloads_only(&frames.lock().unwrap())
            .iter()
            .any(|f| f.first() == Some(&0x31)),
        "the teardown (0x31) must be deferred while held, not run yet"
    );

    // Reconnect. The held function's Reset (0x31) must reach the wire as part of the
    // reconnect — the explicit return-to-safe, not a dropped obligation.
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert!(
        payloads_only(&frames.lock().unwrap())
            .iter()
            .any(|f| f.first() == Some(&0x31)),
        "reconnect did not tear down the held actuation (0x31) on the wire"
    );
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

#[tokio::test]
async fn list_measurements_falls_back_to_the_ista_catalog_without_an_sgbd() {
    // No --sgbd-dir, but the semantic DB carries the measurement catalog: an
    // inline-scaling ECU (no SGBD) still lists — names + units + reading job from
    // ISTA's index, marked source "ista_catalog".
    let (_dir, db) = fixture_db();
    let config = ServerConfig::parse_from(["klartext-mcp", "--semantic-db", db.to_str().unwrap()]);
    let server = KlartextServer::new(config);
    let result = server
        .list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("dde_x".to_string()),
            ecu: None,
            search: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.total, 2);
    assert!(
        result
            .0
            .measurements
            .iter()
            .all(|m| m.source == "ista_catalog"),
        "all entries should be catalog-sourced"
    );
    let temp = result
        .0
        .measurements
        .iter()
        .find(|m| m.name == "STAT_EXAMPLE_TEMP_WERT")
        .unwrap();
    assert_eq!(temp.unit, "°C");
    assert_eq!(temp.job.as_deref(), Some("STATUS_LESEN"));
    assert!(
        result.0.note.contains("ISTA measurement catalog"),
        "{}",
        result.0.note
    );
    // The search filter applies to the catalog too.
    let filtered = server
        .list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("dde_x".to_string()),
            ecu: None,
            search: Some("RPM".to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(filtered.0.total, 1);
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

    // The engine-oil CBS reset: low-risk, derived (unconfirmed) — eligible for a
    // future confirmed-write tool.
    let oil = all.0.functions.iter().find(|f| f.label == "Oel").unwrap();
    assert_eq!(oil.risk, "low");
    assert_eq!(oil.derivation, "derived-unconfirmed");
    assert!(oil.confirmed_write_eligible);
    assert!(oil.citation.as_deref().unwrap().contains("CBS_RESET"));

    // A throttle actuator: high-risk, never eligible via this surface.
    let dro = all.0.functions.iter().find(|f| f.label == "DRO").unwrap();
    assert_eq!(dro.risk, "high");
    assert!(!dro.confirmed_write_eligible);

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

// ── list_service_function_ids: DISCOVERY for run_service_function (P3 Task 8) ──
//
// run_service_function takes an integer catalog `function_id` and nothing else
// surfaces it — list_service_functions returns a DIFFERENT identifier (a string
// `label`) for the SGBD-derived path. This tool is the discovery bridge. A pure
// semantic-DB read: no car connection, no --sgbd-dir, no execution.

#[tokio::test]
async fn list_service_function_ids_surfaces_the_ids_run_service_function_takes() {
    // The SAME fixture the confirmed-write bridge test drives: its function_id 1 is
    // exactly what run_service_function_transmits_a_write_over_the_confirmed_write_bridge
    // passes, so this proves the discovery→run linkage end to end.
    let (_dir, db) = fixture_db_with_service_function();
    let server = KlartextServer::new(config_with_db(&db));
    let out = server
        .list_service_function_ids(Parameters(ListServiceFunctionIdsRequest {
            ecu: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    let r = out.0;
    assert_eq!(r.variant, "d72n47a0");
    assert_eq!(r.count, 1);
    let f = &r.functions[0];
    assert_eq!(
        f.function_id, 1,
        "the id an agent passes to run_service_function"
    );
    assert_eq!(f.title.as_deref(), Some("Electric fan release"));
    // Main-only, no fixed_function row → no hold (matches run_service_function).
    assert!(!f.has_reset);
    assert_eq!(f.hold, "none");
    assert_eq!(f.hold_ms, None);
    // The note tells the agent how to turn a listed id into a run.
    assert!(r.note.contains("run_service_function"), "{}", r.note);
}

#[tokio::test]
async fn list_service_function_ids_classifies_the_hold_and_surfaces_operator_text() {
    // The hold summary an agent needs BEFORE confirming is classified with the SAME
    // rule run_service_function executes (klartext_service::hold_for): a Main+Reset
    // function with Activation==0 HOLDS until stop_service, and ISTA's preparing_text
    // must surface so the human sees the instruction before confirming.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("semantic.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, rank INTEGER, position INTEGER, value TEXT, label TEXT, job TEXT);
         INSERT INTO job_param VALUES ('d72n47a0',7,'Fuel pump: activation',NULL,'Main',1,1,'',NULL,'STEUERN_X');
         INSERT INTO job_param VALUES ('d72n47a0',7,'Fuel pump: activation',NULL,'Reset',1,1,'',NULL,'STEUERN_X');
         CREATE TABLE fixed_function (function_id INTEGER, activation INTEGER, activation_duration_ms INTEGER, preparing_text TEXT, processing_text TEXT, post_text TEXT);
         INSERT INTO fixed_function VALUES (7, 0, NULL, 'Engine off, ignition on.', NULL, NULL);",
    )
    .unwrap();
    let server = KlartextServer::new(config_with_db(&db));
    let out = server
        .list_service_function_ids(Parameters(ListServiceFunctionIdsRequest {
            ecu: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    let f = &out.0.functions[0];
    assert_eq!(f.function_id, 7);
    assert!(f.has_reset);
    // Activation==0 with a Reset → held until an explicit stop (hold_for's UntilStop).
    assert_eq!(f.hold, "until_stop");
    assert_eq!(f.hold_ms, None);
    // ISTA's own operator instruction surfaces for the human to see pre-confirm.
    assert_eq!(
        f.preparing_text.as_deref(),
        Some("Engine off, ignition on.")
    );
}

#[tokio::test]
async fn list_service_function_ids_without_a_db_errors_clearly() {
    // Unlike list_service_functions there is NO SGBD fallback for these ids — they
    // live only in the semantic DB. Point at a path that cannot open so the outcome
    // is deterministic regardless of any BYO DB on disk.
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--semantic-db",
        "/nonexistent/p3-task8-no-such.db",
    ]);
    let server = KlartextServer::new(config);
    let result = server
        .list_service_function_ids(Parameters(ListServiceFunctionIdsRequest {
            ecu: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without a semantic DB, got Ok");
    };
    assert!(err.message.contains("semantic DB"), "{}", err.message);
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
    // The gateway's SVT lists {0x10,0x12,0x40}; 0x18 is in the DB model map but not
    // fitted on this car, so the SVT read excludes it.
    let addrs: Vec<&str> = result
        .0
        .ecus
        .iter()
        .map(|e| e.address_hex.as_str())
        .collect();
    assert_eq!(addrs, ["0x10", "0x12", "0x40"]);
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
    // One entry per fitted ECU. Both DTCs are surfaced now (no client-side filter),
    // and only the one whose status bit 0 is set counts as failing right now.
    assert_eq!(result.0.ecus.len(), 3);
    assert_eq!(result.0.total_faults, 6);
    assert_eq!(result.0.total_present, 3);
    for ecu in &result.0.ecus {
        assert_eq!(ecu.faults.len(), 2, "{}", ecu.address_hex);
        assert_eq!(ecu.faults[0].code_hex, "D9040A");
        assert_eq!(ecu.faults[0].presence, "absent");
        assert_eq!(ecu.faults[1].code_hex, "AABBCC");
        assert_eq!(ecu.faults[1].presence, "present");
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
async fn clear_all_faults_confirmed_runs_istas_ordered_sequence() {
    let (addr, frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    // A short read timeout: the mock deliberately leaves the terminal-15 payloads
    // unanswered (see `spawn_mock_gateway`), so the clamp step ends on a timeout
    // rather than on the real 15-second hold.
    let server = KlartextServer::new(config_for_mock_fast(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    // Scan first so the fitted list is cached. The fixture's fitted set is
    // {0x10, 0x12, 0x40} (MOCK_PRESENT), the gateway itself (0x10) among them.
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
    assert!(
        result.0.note.contains("No ECU was reset"),
        "{}",
        result.0.note
    );
    for ecu in &result.0.ecus {
        assert!(ecu.verified_clean, "{}", ecu.address_hex);
        // Every ECU stored both codes before the clear (the discard record) and
        // nothing after it.
        assert_eq!(
            ecu.codes_before,
            vec!["D9040A".to_string(), "AABBCC".to_string()]
        );
        assert!(ecu.codes_after.is_empty(), "{}", ecu.address_hex);
        assert!(ecu.error.is_none());
    }

    // The broadcast reached 0x12 and 0x40; 0x10 stayed silent and was therefore
    // cleared by the physical straggler pass. Both routes must be visible per ECU —
    // this is the difference between ISTA's sequence and the per-ECU loop klartext
    // ran until 2026-07-18.
    assert_eq!(
        result.0.broadcast_answered,
        vec!["0x12".to_string(), "0x40".to_string()]
    );
    assert!(result.0.broadcast_error.is_none());
    let by = |a: &str| result.0.ecus.iter().find(|e| e.address_hex == a).unwrap();
    assert!(by("0x12").answered_broadcast && !by("0x12").cleared_physically);
    assert!(!by("0x10").answered_broadcast && by("0x10").cleared_physically);

    assert!(result.0.gateway_store_cleared, "the ZFS store must clear");
    assert_eq!(result.0.reidentified.len(), 3);

    // The wire census: the ONE broadcast clear must precede every physical clear,
    // and only the ECU that stayed silent may get one.
    let frames = frames.lock().unwrap().clone();
    let clears: Vec<(u8, Vec<u8>)> = frames
        .iter()
        .filter(|(_, payload)| payload.first() == Some(&0x14))
        .cloned()
        .collect();
    assert_eq!(
        clears,
        vec![
            (0xDF, vec![0x14, 0xFF, 0xFF, 0xFF]),
            (0x10, vec![0x14, 0xFF, 0xFF, 0xFF]),
        ],
        "broadcast first, then only the straggler — got {frames:02X?}"
    );

    // The gateway's combined store, with the value byte the SGBD job actually
    // transmits (00, not the FF its bytecode template shows).
    assert!(
        frames
            .iter()
            .any(|(ecu, p)| *ecu == 0x10 && p.as_slice() == [0x31, 0x01, 0x40, 0x00, 0x00]),
        "the gateway ZFS routine must be sent — got {frames:02X?}"
    );

    // The terminal-15 cycle must be ATTEMPTED. The mock never answers it (that would
    // cost this suite 15 real seconds), so the report shows it failing — but the
    // frame going out is what proves the server did not drop ISTA's step.
    assert!(
        frames
            .iter()
            .any(|(ecu, p)| *ecu == 0x40 && p.as_slice() == klartext_uds::service::clamp::KL15_OFF),
        "the terminal-15 OFF must be transmitted — got {frames:02X?}"
    );
    assert!(!result.0.clamp_cycle.cycled);
    // The OFF never got through, so terminal 15 was never dropped: the SAFE failure.
    assert!(result.0.clamp_cycle.terminal_15_restored);

    // Parity audit P0.1, on the wire: NO ECU is reset, to any address. ISTA's own
    // whole-vehicle clear (VehicleIdent.cs:9720-9788) sends no UDS 0x11 — klartext
    // sent one per cleared ECU until 2026-07-18. The report above could be faked by
    // a broken implementation, so this checks what was actually transmitted. The
    // mock serves `11 01` with a genuine `51 01`, so a reintroduced reset lands here
    // as a real frame rather than a swallowed timeout.
    let reset_targets: Vec<u8> = frames
        .iter()
        .filter(|(_, payload)| payload.first() == Some(&0x11))
        .map(|(ecu, _)| *ecu)
        .collect();
    assert!(
        reset_targets.is_empty(),
        "the clear path must send no ECUReset to any address, got {reset_targets:02X?} \
         in {frames:02X?}"
    );
}

// ── identify_vehicle (M11 Item 2) ─────────────────────────────────────────────

#[tokio::test]
async fn identify_vehicle_returns_vin_and_named_fitted_list() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server.identify_vehicle().await.unwrap();
    // VIN and I-Stufe come from the ZGW identity reads; the FA raw region round-trips.
    assert_eq!(result.0.vin.as_deref(), Some("WBA3B5C50EK123456"));
    assert_eq!(result.0.i_stufe.as_deref(), Some("F020-21-11-500"));
    assert_eq!(result.0.vehicle_order.raw_hex, "AA BB");
    // FA fields stay capture-gated: the raw region is too short to carry a version.
    assert_eq!(result.0.vehicle_order.version, None);

    // The fitted list is the gateway SVT {0x10,0x12,0x40}, named from the DB.
    let addrs: Vec<&str> = result
        .0
        .ecus
        .iter()
        .map(|e| e.address_hex.as_str())
        .collect();
    assert_eq!(addrs, ["0x10", "0x12", "0x40"]);
    let fem = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(fem.group_name.as_deref(), Some("d_0040"));
    assert_eq!(fem.title.as_deref(), Some("Front Electronic Module"));

    // One identification block per fitted ECU; the VIN DID (F190) decodes by name at
    // the surface (the client returns raw), the rest answered negatively and skipped.
    assert_eq!(result.0.identification.len(), 3);
    let vin_field = result
        .0
        .identification
        .iter()
        .flat_map(|b| &b.fields)
        .find(|f| f.did_hex == "F190")
        .expect("VIN identification field present");
    assert_eq!(vin_field.name.as_deref(), Some("VIN"));
    assert_eq!(vin_field.text.as_deref(), Some("WBA3B5C50EK123456"));
}

/// A loopback gateway whose VIN changes per TCP connection: the Nth connection
/// answers `22 F190` with `vins[N]` (the last entry repeats).
///
/// Models the one way a held session's identity can go stale today — klartext has
/// no automatic reconnect, so the agent re-calls `connect`, and the car it lands on
/// need not be the one it left. Any ECU answers, so the ladder stops at the ZGW.
/// `None` means that connection rejects the VIN read on every rung of the ladder.
async fn spawn_mock_gateway_vin_per_connection(vins: &[Option<&str>]) -> std::net::SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let vins: Vec<Option<Vec<u8>>> = vins
        .iter()
        .map(|v| v.map(|v| v.as_bytes().to_vec()))
        .collect();
    tokio::spawn(async move {
        let mut connection = 0usize;
        while let Ok((mut stream, _)) = listener.accept().await {
            let vin = vins[connection.min(vins.len() - 1)].clone();
            connection += 1;
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    let uds = match (frame.payload.as_slice(), vin.as_ref()) {
                        ([0x3E, 0x80], _) => continue,
                        ([0x3E, 0x00], _) => vec![0x7E, 0x00],
                        ([0x22, 0xF1, 0x90], Some(vin)) => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(vin);
                            uds
                        }
                        _ => vec![0x7F, 0x22, 0x31],
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, uds); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    addr
}

/// A loopback gateway where only `holder` answers `22 F190`; everyone else rejects.
///
/// Exercises ISTA's VIN ladder end-to-end through the MCP surface: a car whose
/// gateway holds no VIN is not a car without a VIN.
async fn spawn_mock_gateway_vin_only_on(holder: u8, vin: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    let uds = match frame.payload.as_slice() {
                        [0x3E, 0x80] => continue,
                        [0x22, 0xF1, 0x90] if ecu == holder => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(vin.as_bytes());
                            uds
                        }
                        _ => vec![0x7F, 0x22, 0x31],
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, uds); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn connect_reads_the_vin_from_the_cas_when_the_gateway_holds_none() {
    // ISTA's ladder is ZGW → CAS → FRM, first non-empty wins. klartext read the ZGW
    // alone, so this car reported no VIN at all.
    let (_dir, db) = fixture_db();
    let addr = spawn_mock_gateway_vin_only_on(0x40, "WBA1K2C50EV000000").await;
    let server = KlartextServer::new(config_for_mock(addr, &db));

    let result = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert_eq!(result.0.vin.as_deref(), Some("WBA1K2C50EV000000"));
    assert_eq!(result.0.vin_source, "did_f190");
    // Nothing to compare against on a first connect.
    assert_eq!(result.0.vin_check, None);
}

/// ISTA parity (owner-ruled 2026-07-19): a VIN mismatch is a HARD ABORT with a full
/// disconnect, not a warning. The old session must ALSO be closed — leaving it live
/// would let an agent that ignored the error keep reading a car the cable is no
/// longer on, which is the actual hazard.
#[tokio::test]
async fn reconnecting_to_a_different_car_aborts_and_closes_both_sessions() {
    let (_dir, db) = fixture_db();
    let addr = spawn_mock_gateway_vin_per_connection(&[
        Some("WBA1K2C50EV000000"),
        Some("WBAXXXXXXXXXX9999"),
    ])
    .await;
    let server = KlartextServer::new(config_for_mock(addr, &db));

    let first = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert_eq!(first.0.vin_check, None);

    // Re-connect — klartext's only reconnect — lands on a different car.
    let Err(err) = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
    else {
        panic!("a VIN mismatch must abort the connect, not return Ok");
    };
    // The message must name BOTH cars and say the old findings are void, since
    // acting on them is the hazard this whole check exists to prevent.
    let msg = &err.message;
    assert!(msg.contains("DIFFERENT car"), "{msg}");
    assert!(msg.contains("WBA1K2C50EV000000"), "{msg}");
    assert!(msg.contains("WBAXXXXXXXXXX9999"), "{msg}");

    // ...and the previous session is gone, not merely un-updated. A subsequent read
    // must report "not connected" rather than quietly serving the old car.
    let Err(after) = server
        .read_faults(Parameters(ReadFaultsRequest {
            ecu: "0x40".to_string(),
        }))
        .await
    else {
        panic!("the old session must be closed after a mismatch");
    };
    assert!(after.message.contains("not connected"), "{}", after.message);
}

#[tokio::test]
async fn reconnecting_to_the_same_car_reports_a_vin_match() {
    let (_dir, db) = fixture_db();
    let addr = spawn_mock_gateway_vin_per_connection(&[Some("WBA1K2C50EV000000")]).await;
    let server = KlartextServer::new(config_for_mock(addr, &db));

    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    let second = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert_eq!(second.0.vin_check.as_deref(), Some("match"));
    assert!(
        !second.0.note.contains("DIFFERENT VEHICLE"),
        "{}",
        second.0.note
    );
}

#[tokio::test]
async fn reconnecting_to_a_silent_car_reports_unreadable_not_mismatch() {
    // The distinction that must survive the round trip to the surface: no ECU
    // answered the VIN read, which proves nothing about which car this is.
    // Reporting it as a mismatch would tell the agent the car had been swapped
    // when it may simply be asleep.
    let (_dir, db) = fixture_db();
    let addr = spawn_mock_gateway_vin_per_connection(&[Some("WBA1K2C50EV000000"), None]).await;
    let server = KlartextServer::new(config_for_mock(addr, &db));

    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    let second = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert_eq!(second.0.vin, None);
    assert_eq!(second.0.vin_check.as_deref(), Some("unreadable"));
    assert!(
        !second.0.note.contains("DIFFERENT VEHICLE"),
        "{}",
        second.0.note
    );
}

/// A loopback gateway that answers the VIN (so `connect` succeeds) but REJECTS the
/// SVT installed-ECU read (`22 3F 07`) with a negative response.
///
/// Regression fixture for the no-fallback invariant: a failed SVT read must surface
/// as an error from the discovery tools, never degrade to an empty/partial success.
async fn spawn_mock_gateway_svt_fails() -> std::net::SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    let uds = match frame.payload.as_slice() {
                        [0x3E, 0x80] => continue,         // keepalive — no reply
                        [0x3E, 0x00] => vec![0x7E, 0x00], // presence probe
                        [0x22, 0xF1, 0x90] => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(b"WBA3B5C50EK123456");
                            uds
                        }
                        // The SVT read is REJECTED (requestOutOfRange). The client must
                        // propagate this, not fall back to an empty fitted list.
                        [0x22, 0x3F, 0x07] => vec![0x7F, 0x22, 0x31],
                        _ => continue,
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, uds); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn discovery_errors_when_the_svt_read_fails() {
    // Regression lock (Task 8 review): a failed installed-ECU (SVT) read must surface
    // as an ERROR from the discovery tools, never a degraded empty/partial success.
    let (_dir, db) = fixture_db();
    let addr = spawn_mock_gateway_svt_fails().await;
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // scan_ecus reads the SVT directly — a rejected read is an error, not empty ECUs.
    let scan = server
        .scan_ecus(Parameters(ScanEcusRequest { rescan: false }))
        .await;
    let Err(err) = scan else {
        panic!("expected scan_ecus to error on a failed SVT read, got Ok");
    };
    assert!(err.message.contains("SVT"), "{}", err.message);

    // identify_vehicle aggregates the same SVT read (no probe fallback) — it must fail
    // too, not return a degraded identity with an empty fitted list.
    let ident = server.identify_vehicle().await;
    let Err(err) = ident else {
        panic!("expected identify_vehicle to error on a failed SVT read, got Ok");
    };
    assert!(err.message.contains("vehicle identity"), "{}", err.message);
}
