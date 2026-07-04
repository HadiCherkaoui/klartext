# BMW F-Series Diagnostic Wire-Protocol Reference (UDS / HSFZ / DoIP)

> Scope: a from-scratch Rust client + MCP server speaking UDS over HSFZ (F-series, e.g. F20, 2014) and DoIP (G/i-series), over an ENET cable. No EDIABAS, no vendor libraries. This document is a byte-level protocol reference. Where a field is reverse-engineered or version-dependent, it is marked **[verify against a capture]**.
>
> Primary sources: Scapy automotive contrib (`scapy/contrib/automotive/bmw/hsfz.py`, `uds.py`, `doip.py`), the Wireshark HSFZ dissector (packet-hsfz.h: "HSFZ Dissector / By Dr. Lars Voelker / Copyright 2013-2019 BMW Group, Dr. Lars Voelker / Copyright 2020-2023 Technica Engineering"), uholeschak/ediabaslib, python-udsoncan (pylessard), python-doipclient (Jacob Schaer), ISO 14229-1/-2 (via py-uds, udsoncan, PCAN-UDS manual), ISO 13400-2:2019, and the dissec.to knowledgebase. Forum material (Bimmerfest, BimmerPost) was treated as leads to verify.

## TL;DR
- **The daily read path is well-defined and safe**: open TCP to the ZGW on port 6801, wrap each UDS request in a 6-byte HSFZ header (`u32 length`, `u16 control=0x01`, then `u8 source`, `u8 target`), and send UDS service 0x22/0x19/0x14 etc. Reads (0x22, 0x19) need no security; writes (0x2E FDL coding, 0x2F, 0x31) need the extended session (`10 03`) and sometimes SecurityAccess 0x27.
- **HSFZ (F-series) and DoIP (G/i-series) are both thin UDS-over-Ethernet transports** that differ mainly in framing (HSFZ 6-byte header / 1-byte addresses; DoIP 8-byte header / 2-byte addresses), discovery (HSFZ UDP 6811 ident vs DoIP UDP 13400 vehicle announcement), and that DoIP requires an explicit Routing Activation while HSFZ does not.
- **Confirm the uncertain values on your own car with Wireshark**: exact gateway IP, ECU target addresses, the internal byte layout of the HSFZ 0x11 identification string, alive-check timing, and which DIDs each ECU answers — these are reverse-engineered and must be validated against a capture.

## Key Findings
- HSFZ uses TCP **6801** for diagnostics and UDP/TCP **6811** for control/identification. These ports are confirmed across Scapy bindings, ediabaslib config (`EnetDiagnosticPort=6801`, `EnetControlPort=6811`), BMW's own ISTA firewall port list, and nmap (6801/tcp open).
- The tester address (source) is conventionally **0xF4** (BMW EDIABAS uses `TesterAddress = F4,F5`; Scapy `hsfz_scan` default `source=0xf4`); the central gateway (ZGW) is **0x10**. ECUs sit behind the gateway addressed by a 1-byte logical address.
- DoIP is fully standardized (ISO 13400-2), well-supported in Wireshark, and is the forward-looking transport. A single transport abstraction over both is feasible because the UDS application layer is identical.
- The riskiest gaps are all BMW-proprietary HSFZ internals (the 0x11 ident-string sub-fields and the alive-check direction/interval) — these need capture verification.

## Details

### Part 1 — UDS (ISO 14229) application layer

#### 1.1 Request / response structure
A UDS request is `[SID] [sub-function?] [parameters...]`. The positive response echoes the SID with **+0x40** added (e.g. request 0x22 → positive response 0x62). A negative response is always 3 bytes: `0x7F [original SID] [NRC]`.

If a sub-function is present, bit 7 (0x80) of the sub-function byte is the **suppressPosRspMsgIndicationBit**: when set, the ECU performs the action but suppresses a positive response (it still sends a negative response on error). This is heavily used for TesterPresent (`3E 80`).

#### 1.2 Negative Response Code (NRC) table
Source: ISO 14229-1 Annex; mirrored verbatim in py-uds and udsoncan. Values are hex.

| NRC | Name | Meaning |
|-----|------|---------|
| 0x10 | generalReject | Request rejected, no other NRC applies |
| 0x11 | serviceNotSupported | ECU does not support this SID |
| 0x12 | subFunctionNotSupported | ECU does not support this sub-function |
| 0x13 | incorrectMessageLengthOrInvalidFormat | Wrong length/format for the service |
| 0x14 | responseTooLong | Response would exceed transport capacity |
| 0x21 | busyRepeatRequest | ECU busy, retry the request |
| 0x22 | conditionsNotCorrect | Preconditions not met (wrong state/session) |
| 0x24 | requestSequenceError | Messages issued in wrong order |
| 0x25 | noResponseFromSubnetComponent | A subnet component did not answer in time |
| 0x26 | failurePreventsExecutionOfRequestedAction | A DTC-flagged failure blocks the action |
| 0x31 | requestOutOfRange | Parameter/DID/RID out of range or not supported in session |
| 0x33 | securityAccessDenied | Security strategy not satisfied (need 0x27) |
| 0x34 | authenticationRequired | Insufficient rights for the Authentication state (0x29) |
| 0x35 | invalidKey | Key sent did not match the server's expected key |
| 0x36 | exceededNumberOfAttempts | Too many failed key attempts; locked out |
| 0x37 | requiredTimeDelayNotExpired | Must wait before retrying seed/key |
| 0x38–0x4F | (reserved for secured data transmission) | ISO 15764 / extended data-link security |
| 0x70 | uploadDownloadNotAccepted | Transfer setup refused |
| 0x71 | transferDataSuspended | Data transfer aborted |
| 0x72 | generalProgrammingFailure | Flash/EEPROM write or erase error |
| 0x73 | wrongBlockSequenceCounter | TransferData block counter mismatch |
| 0x78 | requestCorrectlyReceived-ResponsePending | Received OK, still working; wait (resets P2 to P2*) |
| 0x7E | subFunctionNotSupportedInActiveSession | Sub-function valid but not in current session |
| 0x7F | serviceNotSupportedInActiveSession | Service valid but not in current session |
| 0x81 | rpmTooHigh | Engine RPM above allowed limit |
| 0x82 | rpmTooLow | Engine RPM below allowed limit |
| 0x83 | engineIsRunning | Action requires engine off |
| 0x84 | engineIsNotRunning | Action requires engine running |
| 0x85 | engineRunTimeTooLow | Engine not run long enough |
| 0x86 | temperatureTooHigh | Temperature above limit |
| 0x87 | temperatureTooLow | Temperature below limit |
| 0x88 | vehicleSpeedTooHigh | Vehicle moving too fast |
| 0x89 | vehicleSpeedTooLow | Vehicle moving too slow |
| 0x8A | throttle/pedalTooHigh | Pedal position too high |
| 0x8B | throttle/pedalTooLow | Pedal position too low |
| 0x8C | transmissionRangeNotInNeutral | Gear not in neutral |
| 0x8D | transmissionRangeNotInGear | Gear not engaged |
| 0x8F | brakeSwitch(es)NotClosed | Brake pedal must be pressed |
| 0x90 | shifterLeverNotInPark | Shifter not in park |
| 0x91 | torqueConverterClutchLocked | TCC locked |
| 0x92 | voltageTooHigh | Supply voltage too high |
| 0x93 | voltageTooLow | Supply voltage too low |

Ranges to encode in Rust: 0x00 reserved/positive (never sent in a negative response); 0x01–0x0F ISO reserved; 0x38–0x4F reserved for secured data transmission; 0x94–0xEF reserved; 0xF0–0xFF vehicle-manufacturer specific (BMW may define its own here — **[verify against a capture]**).

#### 1.3 Service catalog reference table
Flags: **R** = read-safe (daily safe path); **S** = typically requires SecurityAccess (0x27) and/or non-default session; **W** = write/actuation (changes state — handle with care).

| Service | SID | Pos.resp | Key sub-functions | Request layout | Response layout | Flag |
|---|---|---|---|---|---|---|
| DiagnosticSessionControl | 0x10 | 0x50 | 0x01 default, 0x02 programming, 0x03 extended, 0x04 safetySystem | `10 [session]` | `50 [session] [P2_hi P2_lo P2*_hi P2*_lo]` | R (session change itself is benign; enables W) |
| ECUReset | 0x11 | 0x51 | 0x01 hardReset, 0x02 keyOffOnReset, 0x03 softReset, 0x04 enableRapidPowerShutDown | `11 [type]` | `51 [type] {powerDownTime}` | W |
| ClearDiagnosticInformation | 0x14 | 0x54 | (none) | `14 [DTC_hi DTC_mid DTC_lo]` (0xFFFFFF = all) | `54` | W (clears fault memory) |
| ReadDTCInformation | 0x19 | 0x59 | 0x01 reportNumberByStatusMask, 0x02 reportDTCByStatusMask, 0x04 reportSnapshotByDTC, 0x06 reportExtDataByDTC, 0x0A reportSupportedDTC | `19 [subfn] [args]` | `59 [subfn] [statusAvailMask] [DTC+status records]` | R |
| ReadDataByIdentifier | 0x22 | 0x62 | (none) | `22 [DID_hi DID_lo]...` | `62 [DID_hi DID_lo] [data]...` | R |
| ReadMemoryByAddress | 0x23 | 0x63 | (none) | `23 [ALFID][addr][size]` | `63 [data]` | R (often S) |
| SecurityAccess | 0x27 | 0x67 | odd = requestSeed (0x01,0x03,…), even = sendKey (0x02,0x04,…) | `27 [level] {key}` | `67 [level] {seed}` | S |
| CommunicationControl | 0x28 | 0x68 | 0x00 enableRxTx, 0x01 enableRx/disableTx, 0x03 disableRxTx | `28 [ctrlType] [commType]` | `68 [ctrlType]` | W |
| WriteDataByIdentifier | 0x2E | 0x6E | (none) | `2E [DID_hi DID_lo] [data]` | `6E [DID_hi DID_lo]` | W/S (FDL coding) |
| InputOutputControlByIdentifier | 0x2F | 0x6F | control option: 0x00 returnControl, 0x01 resetToDefault, 0x02 freeze, 0x03 shortTermAdjust | `2F [DID_hi DID_lo] [ctrlOption] [ctrlState] {mask}` | `6F [DID_hi DID_lo] [ctrlOption] [state]` | W (actuation) |
| RoutineControl | 0x31 | 0x71 | 0x01 startRoutine, 0x02 stopRoutine, 0x03 requestRoutineResults | `31 [subfn] [RID_hi RID_lo] [params]` | `71 [subfn] [RID_hi RID_lo] [results]` | W/S (service functions) |
| RequestDownload | 0x34 | 0x74 | (none) | `34 [dataFormat][ALFID][addr][size]` | `74 [lengthFormat][maxBlockLen]` | S (flashing — out of scope) |
| RequestUpload | 0x35 | 0x75 | (none) | `35 [dataFormat][ALFID][addr][size]` | `75 [lengthFormat][maxBlockLen]` | S (out of scope) |
| TransferData | 0x36 | 0x76 | (none) | `36 [blockSeqCounter] [data]` | `76 [blockSeqCounter] {data}` | S (out of scope) |
| RequestTransferExit | 0x37 | 0x77 | (none) | `37 {params}` | `77 {params}` | S (out of scope) |
| WriteMemoryByAddress | 0x3D | 0x7D | (none) | `3D [ALFID][addr][size][data]` | `7D [ALFID][addr][size]` | W/S |
| TesterPresent | 0x3E | 0x7E | 0x00 zeroSubFunction | `3E 00` or `3E 80` (suppressed) | `7E 00` | R (keepalive) |
| ControlDTCSetting | 0x85 | 0xC5 | 0x01 on, 0x02 off | `85 [setting]` | `C5 [setting]` | W (suspends DTC logging) |

Transfer services 0x34/0x35/0x36/0x37 are noted at summary level only; flashing/programming is out of scope beyond acknowledging it exists and is gated by programming session + SecurityAccess.

#### 1.4 Sessions and timing
- **0x01 defaultSession**: always active at power-up; only a limited safe subset of services (notably reads) is guaranteed.
- **0x02 programmingSession**: for flashing; requires SecurityAccess; application stops, only bootloader/DCM runs.
- **0x03 extendedDiagnosticSession**: superset of default; enables writes (0x2E), I/O control (0x2F), routines (0x31), DTC clear, ECU reset. **This is the session you enter for FDL coding and service functions.**
- **0x04 safetySystemDiagnosticSession**: for safety-related ECUs.

Timing (ISO 14229-2:2013 §7.2 Table 4 defaults): **P2_server_max = 50 ms** (`PUDS_P2CAN_SERVER_MAX_DEFAULT = 50`, per the PCAN-UDS 2.x manual) — the time the ECU has to start its response — and **P2*_server_max = 5000 ms** (`PUDS_P2CAN_ENHANCED_SERVER_MAX_DEFAULT = 5000`, in milliseconds) — the extended budget after an NRC 0x78 "response pending." The session keepalive uses **S3Client = 2000 ms** (the ISO 14229-2:2013 §7.2 Table 4 recommended reload value; PCAN-UDS: `PUDS_S3_CLIENT_TIMEOUT_RECOMMENDED = 2000 … Default value in milliseconds for the S3 client performance requirement`), against the **S3Server = 5000 ms** inactivity timeout (tolerance −0/+200 ms) after which the ECU drops back to default session. So the tester sends TesterPresent (`3E 80`) roughly every 2 s to hold a non-default session open. The actual P2/P2* values are reported by the ECU in the DiagnosticSessionControl positive response (4 bytes: P2 in ms, P2* in 10 ms units) — read them rather than hard-coding. **[verify against a capture]** for the specific F20 ECUs.

Implementation note: on NRC **0x78** you must keep waiting (switch your read timeout from P2 to P2*) and loop until a non-0x78 response arrives.

#### 1.5 Data identifiers (DIDs) and DTC format
DID ranges (ISO 14229-1):

| Range | Allocation |
|---|---|
| 0x0000–0x00FF | ISO/SAE reserved |
| 0x0100–0xA5FF | Vehicle-manufacturer specific (BMW custom — most live data here) |
| 0xA600–0xA7FF | reserved for legislative use |
| 0xA800–0xACFF | vehicle-manufacturer specific |
| 0xAD00–0xAFFF | reserved for legislative use |
| 0xB000–0xB1FF | vehicle-manufacturer specific |
| 0xB200–0xBFFF | system-supplier specific (some sub-ranges reserved) |
| 0xC000–0xC2FF | vehicle-manufacturer specific |
| 0xC300–0xCEFF | reserved/legislative |
| 0xCF00–0xEFFF | system-supplier specific |
| 0xF000–0xF00F | network configuration |
| 0xF010–0xF0FF | vehicle-manufacturer specific |
| 0xF100–0xF17F | identification (see below) |
| 0xF180–0xF1FF | identification (standardized 0xF180–0xF1A2; rest OEM) |
| 0xFA00–0xFCFF | system-supplier specific |
| 0xFD00–0xFEFF | system-supplier specific |
| 0xFF00–0xFFFF | ISO/SAE reserved (0xFF00 = UDSVersionDataIdentifier) |

Standard identification DIDs (0xF1xx):
- **0xF190 — VIN** (17 ASCII chars). This is the canonical "read the VIN" DID.
- 0xF180 BootSoftwareIdentification, 0xF181 applicationSoftwareIdentification, 0xF182 applicationDataIdentification.
- 0xF187 vehicleManufacturerSparePartNumber, 0xF188 vehicleManufacturerECUSoftwareNumber, 0xF189 vehicleManufacturerECUSoftwareVersionNumber.
- 0xF18A systemSupplierIdentifier, 0xF18C ECUSerialNumber.
- 0xF191 vehicleManufacturerECUHardwareNumber, 0xF192–0xF195 supplier HW/SW numbers and versions, 0xF197 systemName, 0xF19E ASAM ODX file identifier.
- 0xF1A0–0xF1FF: BMW-specific identification fields. The IP-configuration DID is **0x172A** — confirmed in the dissec.to Scapy capture: `UDS_RDBI(identifiers=[0x172a]) … dataIdentifier= IPConfiguration … IP = 192.168.17.151 SUBNETMASK= 255.255.255.0 DEFAULT_GATEWAY= 192.168.17.1`. Still **[verify against a capture]** on your own car since the exact DID set is ECU/model-specific.

DTC format: BMW F-series uses the UDS **3-byte DTC** (high/mid/low). Each DTC in a 0x19 response is followed by a **1-byte status**:

| Bit | Mask | Name |
|---|---|---|
| 0 | 0x01 | testFailed |
| 1 | 0x02 | testFailedThisOperationCycle |
| 2 | 0x04 | pendingDTC |
| 3 | 0x08 | confirmedDTC |
| 4 | 0x10 | testNotCompletedSinceLastClear |
| 5 | 0x20 | testFailedSinceLastClear |
| 6 | 0x40 | testNotCompletedThisOperationCycle |
| 7 | 0x80 | warningIndicatorRequested |

The 0x19 0x02 request carries a **status mask**; the ECU returns only DTCs whose status byte ANDed with the mask is non-zero. `19 02 08` = "all confirmed DTCs" (the standard workshop scan). `19 02 FF` = everything. Note BMW's human-facing fault codes (e.g. hex like 0x4202F1 displayed in ISTA) are an ECU-internal/ISTA representation; the raw UDS DTC is the 3-byte value — map between them per ECU **[verify against a capture]**.

#### 1.6 BMW gateway VCM read DIDs (SVT / FA / I-Stufe)
The central gateway (ZGW, logical **0x10**) exposes the vehicle's configuration through its VCM (Vehicle Configuration Management) as manufacturer-range DIDs, all read with **0x22 ReadDataByIdentifier** — no session and no SecurityAccess, so they are autonomous-safe reads:

| DID | Content | EDIABAS read job |
|---|---|---|
| **0x3F07** | Installed-ECU list — the **SVT** (*System-Verbau-Tabelle*): the diagnostic addresses of every ECU actually fitted. The source of truth for a whole-car scan (it replaces address probing). | `STATUS_VCM_GET_ECU_LIST_ALL` |
| **0x3F06** | Vehicle order — the **FA** (*Fahrzeugauftrag*): the car's build order (series/type, paint, upholstery, options). | `STATUS_VCM_GET_FA` |
| **0x100B** | **I-Stufe** (integration level): the vehicle's software-build stamp, ASCII. | `STATUS_VCM_I_STUFE_LESEN` |

klartext reads all three from 0x10 for its `identify` / `identify_vehicle` surface. The positive-response record layouts (SVT count + address stride, the FA version-byte offset and header fields, the I-Stufe string framing) are derived from ISO + SGBD disassembly and are **[verify against a capture]** — the 2026-07-03 F20 pcap carries no `0x22 3F07 / 3F06 / 100B` traffic.

**Read vs. control — the EDIABAS job-class prefix.** A BMW SGBD job name declares its blast radius by prefix: **`STATUS_*`** jobs read and emit **0x22** (or **0x19** for fault memory); **`STEUERN_*`** jobs control/actuate and emit **0x31 RoutineControl** (or **0x2E WriteDataByIdentifier**). The three DIDs above are the `STATUS_VCM_*` read side. Their `STEUERN_VCM_*` counterparts write and stay out of the autonomous surface — e.g. `STEUERN_VCM_GENERATE_SVT` (a 0x31 job that regenerates the SVT) is deliberately not exposed. See §4.2 for the fuller job→UDS mapping.

### Part 2 — HSFZ (BMW proprietary, F-series transport)

HSFZ = *High-Speed-Fahrzeug-Zugang* ("high-speed vehicle access"). Per the Wireshark HSFZ dissector merge request by Dr. Mickey Lauer (March 2023): "HSFZ encapsulates standard UDS packets … It is a proprietary protocol that has been in use since the late 2000s, and by now shipped in millions of vehicles all around the world." It is carried over the physical ENET interface. Authoritative open sources: Scapy `scapy/contrib/automotive/bmw/hsfz.py` (Nils Weiss), the Wireshark HSFZ dissector (packet-hsfz.h: "Copyright 2013-2019 BMW Group, Dr. Lars Voelker / Copyright 2020-2023 Technica Engineering, Dr. Lars Voelker"), and uholeschak/ediabaslib.

#### 2.1 Frame layout (byte-level)

| Field | Offset | Width | Endian | Meaning |
|---|---|---|---|---|
| LENGTH | 0 | 4 bytes | big-endian | Number of payload bytes = 2 (control word) + SRC + TGT + UDS data. **Does not** include the 4-byte length field itself. |
| CONTROL | 4 | 2 bytes | big-endian | Control word / message type (see table) |
| SOURCE | 6 | 1 byte | — | Tester logical address (present for control 0x01, 0x02, and 2-byte alive checks) |
| TARGET | 7 | 1 byte | — | Target ECU logical address (same condition) |
| PAYLOAD | 8 | LENGTH−2 | — | UDS message (for control 0x01/0x02), or identification string (0x11), etc. |

So a UDS request frame on the wire is: `00 00 00 LL  00 01  SA  TA  [UDS bytes]`, where `LL = 2 + len(UDS)`. Total bytes on wire = LENGTH + 4.

Reading from the TCP stream: peek 4 bytes for LENGTH, then read the rest — TCP is a byte stream, so HSFZ frames can split across or coalesce within TCP segments; buffer accordingly. Scapy's `HSFZSocket.recv` does exactly this (peek the 4-byte length via `MSG_PEEK`, then read the remaining bytes). Scapy's `post_build` sets `length = len(payload) + 2`.

For error frames (control 0x40 incorrect_tester_address) the two address bytes are **EXPECTED** then **RECEIVED** rather than SOURCE/TARGET.

#### 2.2 Control words (message types)
Verbatim from Scapy `HSFZ.control_words`:

| Control | Name | Channel | Meaning |
|---|---|---|---|
| 0x01 | diagnostic_req_res | TCP 6801 | Diagnostic message (carries UDS, plus SRC/TGT) |
| 0x02 | acknowledge_transfer | TCP 6801 | Acknowledge/echo of a diagnostic message |
| 0x10 | terminal15 | control | Terminal 15 (ignition/clamp-15) signal |
| 0x11 | vehicle_ident_data | UDP 6811 | Vehicle identification / announcement (VIN etc.) |
| 0x12 | alive_check | TCP 6801 | Keepalive / alive check |
| 0x13 | status_data_inquiry | control | Status / data inquiry |
| 0x40 | incorrect_tester_address | — | Error: wrong tester address (carries EXPECTED/RECEIVED) |
| 0x41 | incorrect_control_word | — | Error: invalid control word |
| 0x42 | incorrect_format | — | Error: invalid format |
| 0x43 | incorrect_dest_address | — | Error: invalid destination address |
| 0x44 | message_too_large | — | Error: message too large |
| 0x45 | diag_app_not_ready | — | Error: diagnostic application not ready |
| 0xFF | out_of_memory | — | Error: out of memory |

The dissec.to writeup uses a simplified mapping ("TYPE: 1 is message, 2 is echo/ack, 64 is error"), consistent with the above.

#### 2.3 Ports
- **TCP 6801** — diagnostic channel (`EnetDiagnosticPort` / `DiagnosticPort`). Confirmed by Scapy bindings, ediabaslib config, BMW ISTA firewall port list, and nmap.
- **UDP/TCP 6811** — control channel (`EnetControlPort` / `ControlPort`). UDP 6811 is used for the identification broadcast; EDIABAS/ICOM also opens a TCP control connection on 6811.
- With a BMW ICOM the diagnostic/control ports are reassigned at runtime (e.g. Diag Port 50160, Control Port 50161, per ediabaslib's config doc) — **[verify against a capture]** if using an ICOM rather than a plain ENET cable.

#### 2.4 Addressing / routing behind the ZGW
1-byte logical addresses live inside the HSFZ header. Conventions confirmed in BMW reverse-engineering work:
- **Tester (source) = 0xF4** (BMW EDIABAS uses F4,F5; Scapy default 0xF4).
- **Central gateway ZGW = 0x10**.
- DME (engine) = 0x12, CAS (Car Access System) = 0x40, etc. The full set is ECU- and model-specific; enumerate by scanning targets 0x00–0xFF and seeing which answer a `10 03` DSC. **[verify against a capture]** for your F20.

To address an ECU behind the gateway you simply set TARGET to that ECU's logical address; the ZGW routes the UDS payload onto the correct internal CAN/FlexRay bus and relays the response back with SRC/TGT swapped. On F-series, internal CAN frames use IDs of the form 0x6nn where nn is the ECU ID (response on 0x6F4 toward the tester), with the ZGW handling ISO-TP segmentation internally so the tester deals only with whole HSFZ frames.

#### 2.5 Connection / identification handshake (implementable sequence)
1. **Discover the gateway.** Broadcast an HSFZ **vehicle identification request (control 0x11)** on **UDP 6811** to the subnet broadcast. With an unconfigured link-local setup the ZGW/FEM uses a 169.254.0.0/16 link-local address, so broadcast to 169.254.255.255. A minimal observed discovery datagram is `00 00 00 00 00 11` (length 0, control 0x11). The gateway replies with a 0x11 **announcement** containing the VIN and identification data, from which you learn its IP and logical address.
2. **Parse the 0x11 identification string** to extract VIN / logical address / MAC / IP. Scapy models this body only as an opaque `identification_string`; the exact internal sub-field offsets are **not** documented in open sources and **[verify against a capture]** (or against ediabaslib's `EdInterfaceEnet.cs` parser at `EdiabasLib/EdiabasLib/EdInterfaceEnet.cs`).
3. **Open the diagnostic TCP connection** to the gateway IP on **port 6801** (set `TCP_NODELAY`). Connect timeout default is 5000 ms in ediabaslib (`EnetTimeoutConnect`); BMW's stock EDIABAS.INI uses 20000 ms — a conflict worth noting, so make it configurable.
4. **No routing activation is required** (unlike DoIP) — you may send UDS immediately after TCP connect.
5. **Send UDS** wrapped in control 0x01 frames with your SOURCE=0xF4 and TARGET=ECU; the ECU/gateway replies with control 0x01 frames (and may echo/ack with 0x02).
6. **Keepalive**: the link is kept alive with control **0x12 alive_check** messages and by UDS TesterPresent (`3E 80`) within S3 (~5 s; send every ~2 s). HSFZ has two alive-check forms: a **2-byte** form (LENGTH=2: control 0x12 + SOURCE + TARGET, 8 bytes on the wire) and a **longer** form carrying an identification string. The exact sender (gateway-initiated vs tester-initiated) and interval are **not** pinned down in open sources — **[verify against a capture]**; in practice, sending UDS `3E 80` periodically is sufficient to hold the session.

### Part 3 — DoIP (ISO 13400-2, forward-looking transport for G/i-series)

#### 3.1 Generic header layout (byte-level)

| Field | Offset | Width | Meaning |
|---|---|---|---|
| Protocol Version | 0 | 1 byte | 0x01 = ISO 13400-2:2010, **0x02 = 2012**, 0x03 = 2019; 0xFF = default for vehicle-ident requests |
| Inverse Protocol Version | 1 | 1 byte | bitwise inverse of byte 0 (e.g. 0xFD for 0x02) — sync/validation pattern |
| Payload Type | 2 | 2 bytes | message type (see catalog) |
| Payload Length | 4 | 4 bytes | length in bytes of the payload that follows (big-endian) |
| Payload | 8 | variable | depends on payload type |

For a **diagnostic message** (payload type 0x8001) the payload is: `[Source Address (2 bytes)] [Target Address (2 bytes)] [UDS data...]`. So a full UDS-over-DoIP frame is `02 FD 80 01 [len:4] [SA:2] [TA:2] [UDS]`.

#### 3.2 Payload-type catalog

| Payload type | Name | Transport |
|---|---|---|
| 0x0000 | Generic DoIP header negative acknowledge | UDP/TCP |
| 0x0001 | Vehicle identification request | UDP |
| 0x0002 | Vehicle identification request with EID | UDP |
| 0x0003 | Vehicle identification request with VIN | UDP |
| 0x0004 | Vehicle announcement / vehicle identification response | UDP |
| 0x0005 | Routing activation request | TCP |
| 0x0006 | Routing activation response | TCP |
| 0x0007 | Alive check request | TCP |
| 0x0008 | Alive check response | TCP |
| 0x4001 | DoIP entity status request | UDP |
| 0x4002 | DoIP entity status response | UDP |
| 0x4003 | Diagnostic power mode information request | UDP |
| 0x4004 | Diagnostic power mode information response | UDP |
| 0x8001 | Diagnostic message (UDS) | TCP |
| 0x8002 | Diagnostic message positive acknowledgement | TCP |
| 0x8003 | Diagnostic message negative acknowledgement | TCP |

Port: **13400** for both UDP discovery and TCP data (TLS data on 3496 in 2019+).

#### 3.3 Routing activation
- **Request (0x0005)** payload: `[Source Address (2)] [Activation Type (1)] [Reserved ISO (4 = 0x00000000)] {Reserved OEM (4, optional)}`. Activation types: 0x00 Default, 0x01 WWH-OBD, 0xE0 CentralSecurity.
- **Response (0x0006)** payload: `[Logical Address Tester (2)] [Logical Address DoIP entity (2)] [Response Code (1)] [Reserved ISO (4)] {Reserved OEM (4)}`.

Routing activation response codes:

| Code | Meaning |
|---|---|
| 0x00 | Denied — unknown source address |
| 0x01 | Denied — all sockets registered/active |
| 0x02 | Denied — SA different from already-activated socket entry |
| 0x03 | Denied — SA already registered on a different socket |
| 0x04 | Denied — missing authentication |
| 0x05 | Denied — rejected confirmation |
| 0x06 | Denied — unsupported routing activation type |
| 0x10 | **Routing successfully activated** |
| 0x11 | Routing will be activated; confirmation required |

#### 3.4 Vehicle announcement / discovery and handshake
1. Open a UDP socket to port 13400; send a **vehicle identification request** (0x0001, or 0x0002 with EID, or 0x0003 with VIN) to the (limited) broadcast address. On power-up the entity also spontaneously announces itself: per the python-doipclient docs, "When an ECU first turns on, it's supposed to broadcast a Vehicle Announcement Message over UDP 3 times to assist DoIP clients in determining ECU IP's and Logical Addresses." Each announcement is sent after a random initial delay and at a fixed interval (see timing below).
2. The 0x0004 response carries VIN (17 bytes), logical address (2), EID (6, usually MAC), GID (6), further-action-required byte (0x00 none / 0x10 routing activation required for central security), and VIN/GID sync status (0x00 synchronized / 0x10 not synchronized).
3. Open **TCP** to port 13400.
4. **Immediately send a Routing Activation request (0x0005)** and wait for code 0x10. This is mandatory before any diagnostic message — the key difference from HSFZ.
5. Send UDS as **diagnostic message (0x8001)**; each is confirmed by the entity with a **positive (0x8002)** or **negative (0x8003)** ack before the actual UDS response arrives.
6. Keepalive via **alive check request/response (0x0007/0x0008)** and UDS TesterPresent; the entity runs an inactivity timer.

DoIP timing constants (ISO 13400-2:2019 Table 12): **A_DoIP_Ctrl = 2 s** — "the maximum time that the client DoIP entity waits for a response to a previously sent UDP message"; **A_DoIP_Announce_Wait = random 0 to 500 ms** — the initial delay before responding to a vehicle identification request and before transmitting a vehicle announcement after a valid IP address is configured; **A_DoIP_Announce_Interval = 500 ms** between successive announcements.

#### 3.5 HSFZ vs DoIP — what a transport abstraction must expose

| Concern | HSFZ (F-series) | DoIP (G/i-series) |
|---|---|---|
| Discovery | UDP 6811, HSFZ control 0x11 ident request/announcement | UDP 13400, vehicle identification request (0x0001/2/3) + announcement (0x0004) |
| Data port | TCP 6801 | TCP 13400 (TLS 3496) |
| Header size | 6 bytes (4 len + 2 control) | 8 bytes (1 ver + 1 inv + 2 type + 4 len) |
| Version/sync field | none | protocol version + inverse |
| Addressing | 1-byte source/target inside header | 2-byte source/target inside diagnostic-message payload |
| Connection setup | TCP connect → send UDS immediately | TCP connect → **routing activation (0x0005/0x0006)** → send UDS |
| Per-message ack | optional 0x02 acknowledge | mandatory 0x8002/0x8003 ack before UDS response |
| Keepalive | control 0x12 alive check + UDS 3E | 0x0007/0x0008 alive check + UDS 3E |
| Error signalling | control 0x40–0x45, 0xFF | generic header NACK (0x0000), diag NACK (0x8003) |

A clean Rust `Transport` trait should expose: `discover() -> Vec<Entity{ip, logical_addr, vin}>`, `connect(ip)`, `activate()` (no-op for HSFZ, routing activation for DoIP), `send_uds(target, &[u8]) -> Vec<u8>` (hiding framing, acks, and 0x78 response-pending loops), and a background `keepalive()` task. Addresses are `u8` for HSFZ and `u16` for DoIP, so model the logical address as a width-agnostic newtype.

### Part 4 — BMW/gateway specifics

#### 4.1 ZGW routing on F-series
The ZGW (central gateway, logical 0x10; on later body-domain architectures BDC/FEM may take this role) terminates the Ethernet/HSFZ connection and bridges to the internal CAN/FlexRay buses. The tester addresses an internal ECU by its 1-byte logical address in the HSFZ TARGET field; the gateway maps that to the internal bus and CAN identifiers (observed pattern: request ID 0x6nn carrying `F4 TA …`, response on 0x6F4), performing ISO-TP segmentation on the internal side so the tester sees only complete HSFZ frames. Discover the live address map by scanning 0x00–0xFF and noting which targets answer.

#### 4.2 EDIABAS jobs → UDS mapping (for translating ISTA recipes)
EDIABAS "jobs" (from an ECU's SGBD/`.PRG`, driven by ISTA/E-Sys) are named procedures that ultimately emit raw UDS on the wire. Conceptually:
- "read identification / read fault memory" jobs → 0x22 / 0x19 sequences.
- `STATUS_LESEN`-type jobs → 0x22 ReadDataByIdentifier (live values) or 0x19.
- `STEUERN`-type (control) jobs → 0x2F InputOutputControlByIdentifier or 0x31 RoutineControl.
- `FS_LOESCHEN` (clear fault memory) → 0x14 ClearDiagnosticInformation.
- FDL coding (E-Sys, writing a CAFD/NCD into an ECU) → enter extended session (`10 03`), then 0x2E WriteDataByIdentifier (BMW's `WDBI_PLAIN` = "WriteDataByIdentifier with unlimited Data-ID (plain hex value)"); a bad coding payload returns NRC 0x31 requestOutOfRange (observed verbatim in E-Sys transaction logs).

To translate an ISTA/E-Sys recipe: capture the job, identify the SID/DID/RID it emits, and reproduce that raw UDS. Reads are the safe daily path; coding writes need the right session and correct DID/value, and (for some ECUs) SecurityAccess. Note BMW's coding model: **VO coding** resets an ECU's coding to factory defaults derived from the vehicle order; **FDL coding** overwrites individual attributes within a CAFD without altering the VO — both ultimately land as 0x2E writes.

### Part 5 — Reference implementations (what to study in each)

- **Scapy automotive contrib** — `scapy/contrib/automotive/bmw/hsfz.py`, `uds.py`, `doip.py`. Crib: the exact HSFZ field layout, the control-word enum, the stream-reassembly logic in `HSFZSocket.recv` (peek 4-byte length via `MSG_PEEK`, then read the rest), the `post_build` length computation (`len(pay)+2`), and `UDS_HSFZSocket`'s send wrapping (`HSFZ(source, target)/UDS`). For DoIP, study `doip.py` for the payload-type enum, routing-activation response strings, and the conditional-field layout per payload type. For UDS, study the service/sub-function class structure — note Scapy deliberately leaves RDBI/WDBI data records OEM-defined.
- **ediabaslib (uholeschak)** — implements ENET/HSFZ/DoIP for real BMWs. Crib: `EdiabasLib/EdiabasLib/EdInterfaceEnet.cs` (the ENET transport — connection setup, port handling, ident parsing, alive/keepalive), the config doc `docs/EdiabasLib.config_file.md` (port defaults 6801/6811/13400, `EnetTimeoutConnect=5000`, ICOM port reassignment), and the DoIP S29 certificate handling for newer cars. This is the most production-realistic reference for the proprietary bits — and the place to resolve the two HSFZ unknowns (0x11 layout, alive-check timing).
- **python-udsoncan (pylessard)** — the cleanest UDS application-layer model. Crib: per-service request/response builders and `interpret_response`, the SecurityAccess odd/even seed-key handling and `normalize_level`, the DID codec configuration pattern, session/timeout config (`p3_client` drives automatic TesterPresent), and NRC handling. Good blueprint for your Rust service layer API.
- **python-doipclient (Jacob Schaer)** — pure-Python DoIP client. Crib: the message classes (`VehicleIdentificationResponse`, `RoutingActivationRequest/Response`, `DiagnosticMessage` + ack/nack), the further-action/sync-status enums (FurtherActionCodes, SynchronizationStatusCodes), discovery via the vehicle-identification broadcast, and the ENET cable/pinout notes (it documents BMW's ENET cable as DoIP "Option 1," using 100BASE-TX and OBD pin 8 as the Ethernet activation line). Pairs well with udsoncan as the transport under a UDS client.

Also useful: the **Wireshark** DoIP dissector (full ISO 13400 field coverage) and the newer **HSFZ** dissector (Dr. Lars Voelker / Dr. Mickey Lauer) for validating your own captures; the **gallia** project (`gallia.transports.doip`) for a clean async DoIP state machine with the routing-activation response-code enum; and the **dissec.to** knowledgebase for worked Scapy examples against a real BMW (it shows reading 0x172A IPConfiguration over HSFZ).

## Recommendations
1. **Build read-only over HSFZ first.** Implement: UDP 6811 discovery → parse gateway IP/VIN → TCP 6801 → `10 03` (extended session) → `22 F190` (VIN) and `19 02 08` (confirmed DTCs). This exercises the whole stack with zero risk. Benchmark to advance: you get a positive `62 F1 90 …` and a `59 02 …` back.
2. **Verify every proprietary value against a Wireshark capture before trusting it** (Part 6). Threshold to proceed to writes: you can reproduce ISTA/E-Sys's own read traffic byte-for-byte.
3. **Add the keepalive + 0x78 loop early.** A correct `send_uds` that (a) sends `3E 80` roughly every 2 s (within S3 = 5 s) and (b) re-arms its read timeout from P2 (50 ms) to P2* (5000 ms) on NRC 0x78 will save you from intermittent session drops. Threshold: a 10-minute idle session stays in extended session.
4. **Only then add writes (FDL coding via 0x2E) and actuation (0x2F/0x31).** Gate them behind an explicit "unsafe" flag in the MCP server, require the extended session, and always read-back the DID after writing. If you get NRC 0x33/0x35, implement SecurityAccess (0x27) seed/key for that ECU — the algorithm is ECU-specific and must be reverse-engineered or sourced.
5. **Design the transport trait to cover both HSFZ and DoIP from day one** (Part 3.5) so the G/i-series path is a drop-in later. Change trigger: when you target a G-series car, implement DoIP routing activation and 2-byte addressing behind the same trait.
6. **For the MCP server**, expose read tools (read_dtc, read_did, clear_dtc, list_ecus) as first-class and safe; mark coding/actuation tools as requiring explicit confirmation. This matches the read-heavy daily workflow with occasional FDL coding and routine-control service functions.

## Caveats
- **HSFZ is reverse-engineered.** The header layout, ports, control words, and addressing are well-corroborated (Scapy + Wireshark + ediabaslib + BMW's own port list), but the **internal byte layout of the 0x11 identification string** and the **alive-check (0x12) direction/interval** are not nailed down in open sources. Treat them as hypotheses to confirm against a capture or ediabaslib's `EdInterfaceEnet.cs`.
- **Exact ECU logical addresses for the F20 vary by build.** 0xF4 tester / 0x10 ZGW / 0x12 DME / 0x40 CAS are conventions, not guarantees — scan and confirm.
- **P2/P2* and S3 values** quoted (50 ms / 5000 ms server; 2000 ms client / 5000 ms server S3) are ISO 14229-2:2013 defaults; the F20 ECUs report their own P2/P2* in the `10 03` response — read them.
- **Connect-timeout conflict**: ediabaslib defaults to 5000 ms, stock EDIABAS.INI to 20000 ms — make it configurable.
- **NRC range 0xF0–0xFF and some DID ranges** may carry BMW-specific meanings not in ISO 14229; verify against captures.
- **SecurityAccess algorithms are ECU-specific** and are not covered here; without the correct seed→key function, write/coding on protected ECUs will fail with NRC 0x35.
- **Flashing/programming (0x34–0x37, programming session) is deliberately out of scope** — it can brick ECUs and needs signed data; this document only notes it exists.
- Forum-sourced details (Bimmerfest reverse-engineering, ECU name lists, FDL coding workflow) were treated as leads; the load-bearing protocol facts are anchored to Scapy/Wireshark/ISO/ediabaslib. Still, **confirm on your own vehicle.**

## Part 6 — "Verify against a capture" checklist
Run Wireshark on the ENET interface (the HSFZ and DoIP dissectors decode these) while ISTA/E-Sys talks to the car, and confirm:
1. **HSFZ ports** — that diagnostics really are on TCP 6801 and identification on UDP 6811 for your car (ICOM setups reassign these).
2. **HSFZ header endianness & length semantics** — that LENGTH is big-endian and equals `2 + SRC + TGT + UDS` (i.e. excludes the 4 length bytes).
3. **The 0x11 identification-string internal layout** — exact offsets of VIN, logical address, MAC, and IP within the announcement.
4. **Alive-check (0x12) behavior** — who sends it, the 2-byte vs long form used, and the interval; and whether `3E 80` alone holds the session.
5. **Gateway IP and the ECU logical-address map** — the ZGW/FEM IP (likely 169.254.x.x link-local) and which TARGET addresses answer (0x10, 0x12, 0x40, …) on your F20.
6. **P2 / P2* / S3 in practice** — the values the ECUs report in the `10 03` response and how often ISTA sends TesterPresent.
7. **Which DIDs each ECU actually answers** — confirm 0xF190 (VIN), the BMW IP-config DID 0x172A, and the live-data DIDs you care about; many are ECU-specific.
8. **DTC numbering** — the mapping between the raw 3-byte UDS DTC and BMW's displayed hex fault code.
9. **FDL coding round-trip** — capture an E-Sys `2E` write and its response, and confirm the session/security preconditions, before reproducing it yourself.
10. **DoIP (if/when on a G/i car)** — protocol version byte (0x02 vs 0x03), that routing activation returns 0x10, and the 2-byte addresses in 0x8001 messages.

---
### Source URLs
- Scapy HSFZ: https://github.com/secdev/scapy/blob/master/scapy/contrib/automotive/bmw/hsfz.py
- Scapy HSFZ API docs: https://scapy.readthedocs.io/en/latest/api/scapy.contrib.automotive.bmw.hsfz.html
- Scapy DoIP: https://github.com/secdev/scapy/blob/master/scapy/contrib/automotive/doip.py
- Scapy automotive docs: https://scapy.readthedocs.io/en/latest/layers/automotive.html
- dissec.to HSFZ/DoIP knowledgebase: https://munich.dissec.to/kb/chapters/doip/doip.html
- Wireshark HSFZ dissector MR: https://gitlab.com/wireshark/wireshark/-/merge_requests/10101
- ediabaslib config doc: https://github.com/uholeschak/ediabaslib/blob/master/docs/EdiabasLib.config_file.md
- ediabaslib ENET transport (verify HSFZ internals): https://github.com/uholeschak/ediabaslib/blob/master/EdiabasLib/EdiabasLib/EdInterfaceEnet.cs
- python-udsoncan: https://github.com/pylessard/python-udsoncan and https://udsoncan.readthedocs.io
- python-doipclient: https://python-doipclient.readthedocs.io/en/latest/messages.html
- gallia DoIP transport: https://gallia.readthedocs.io/en/stable/_modules/gallia/transports/doip.html
- py-uds NRC / DTC / timing: https://uds.readthedocs.io/en/latest/pages/knowledge_base/
- ISO 13400-2:2019 (Table 12 timing, header): https://standards.iteh.ai/catalog/standards/iso/f1b09c0b-64d0-4493-8d7f-f00d58ae9ed6/iso-13400-2-2019
- BMW ISTA TCP/IP port list: https://aos.bmwgroup.com/api/v2/downloads?key=port-list%2FTCP_IP_postlist_20_1_EN.pdf
- BMW ENET reverse-engineering (lead): https://www.bimmerfest.com/threads/enet-can-diagnostic-messages-for-bench-coding.1398888/
- ELM327 BMW HSFZ extension notes: https://quantexlab.com/en/develop/elm327_hsfz.html