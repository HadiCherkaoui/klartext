# P1 — Resilience parity: retry, serialisation, reconnect + VIN re-validation

> **Research record, 2026-07-18.** Produced under the 1:1 ISTA parity mandate
> (see `CLAUDE.md`): every claim is cited to decompiled ISTA source, ECU `.prg`
> bytecode disassembled with klartext's own `klartext-best`, the shipped
> `EDIABAS.INI`, or a query against real data. Citations reference files under a
> local `<scratchpad>` produced by `ilspycmd` over `data/TesterGUI/bin/Release/`
> and by `klartext_best::decode_job` over `data/Testmodule(1)/Ecu/*.prg` — BYO
> data, gitignored, never committed. Regenerate them locally to follow a citation.
>
> This is a research record, not a plan of record. Where it corrects the parity
> audit, the audit has been updated; where it recommends work, that work is
> tracked separately.


Research spec for klartext. Implementation-ready: findings with citations, then a
concrete change list, then the on-car test procedure.

**Status:** research only. No production code was written; no commits made.
**Date:** 2026-07-18.
**Scope:** parity items P1.1 (retry), P1.2 (serialisation), P1.3 (reconnect + VIN re-check).

---

## 0. Root-cause verdict (read this first)

**The 10-of-32 dropouts are caused by klartext's own request concurrency
(`buffer_unordered(8)`). They are not sleepy ECUs, and they are not a missing NRC
`0x78` handler.**

This is settled by a natural experiment that already exists inside the owner's own
capture (`captures/captures/car-session-1.pcapng`), on the same car, in the same
session, ~160 seconds apart:

| Sweep | in-flight depth | requests | lost | loss rate |
|---|---|---|---|---|
| Early ident sweep (`22 F1 90`) | 0 (strictly sequential) | **388** | **0** | **0 %** |
| Whole-car fault sweep (`19 02`) | 3–7 (`buffer_unordered(8)`) | 32 | 10 | **31 %** |

Broken down by how many requests were already in flight when each `19 02` was sent:

```
 in-flight | sent | answered | LOST | loss%
       0   |    1 |        1 |    0 |     0%
       1   |    1 |        1 |    0 |     0%
       3   |    2 |        0 |    2 |   100%
       5   |    1 |        1 |    0 |     0%
       6   |    5 |        3 |    2 |    40%
       7   |   22 |       16 |    6 |    27%
  TOTAL sent=32 LOST=10 (31%)
```

Loss is **zero at depth 0–1** and **27–100 % at depth ≥ 3**. Reproduce with
`scratchpad/corr2.py` (bounded-occupancy version; the naive version that treats a
lost request as outstanding forever gives the same conclusion).

Three further facts kill the competing hypotheses outright:

1. **Not sleepy ECUs.** Every one of the ten "silent" addresses
   (`06 08 18 19 29 2c 30 5d 67 72`) answered **12–15 times** elsewhere in the same
   41-minute capture, including `62 F1 90` VIN reads at t≈36–42 s. Address `0x29`
   answered a standalone `19 02` in **26.2 ms at t=192.134**, then went silent under
   fan-out at t=202.216 — ten seconds later, same session, same car.
2. **Not a missing `0x78` handler.** klartext implements responsePending correctly —
   `crates/client/src/session.rs:241-255` (re-arms the timeout, bounded by
   `MAX_PENDING_TICKS = 10` at `:52`) and `:318-333` (routing, with the echoed-SID
   check). The capture contains exactly **four** real `0x78` responses
   (`7f2278` ×2 from `0x29` at t≈38.9/39.3, `7f1978` from `0x61` at t=207.192,
   `7f1078` from `0x63` at t=2025.528 in the clear sequence) and all four were
   handled — `docs/car-session-1-results.md:232` already records the clear-sequence
   one as "handled". This hypothesis from the brief is **falsified**.
3. **The gateway signals the overload, and klartext throws the signal away.** The ZGW
   acks accepted diagnostic frames with HSFZ control word `0x02`
   (`docs/protocol-reference.md:198`) at **median 0.59 ms / p95 1.56 ms**. Across the
   whole capture **1 507 of 1 517** TX diagnostic frames were acked (99.34 %). Only
   **10** were never acked — and **5 of those 10 are exactly the burst-3 losses**
   (`18 19 29 67 72`). The gateway refused them at the HSFZ layer. klartext discards
   every ack frame unread (`crates/client/src/session.rs:296-298`:
   `if frame.control != control::DIAGNOSTIC { return; }`), so instead of failing in
   ~2 ms it burns the full 5 s `read_timeout`
   (`P2_STAR_SERVER_MAX_DEFAULT_MS = 5000`, `crates/uds/src/lib.rs:128`).

   The remaining 5 lost requests (`06 08 2c 30 5d`) *were* acked but never answered —
   accepted by the gateway, lost downstream on the bus. So the overload is two-tier:
   gateway admission control **and** bus/ECU-level response loss.

**Cost of the fix is negligible.** At the latencies actually observed
(`19 02` round-trip: median 162 ms, p90 394 ms, max 437 ms, n=29), a strictly
sequential 32-ECU fault sweep costs **≈ 4.8 s**. The early sequential ident sweep did
**388 requests in 6.41 s (16.5 ms/request) with zero loss**. Concurrency is buying
roughly three seconds and paying 31 % data loss for it.

> Caveat on the ack signal: `docs/protocol-reference.md:309` lists the `0x02` ack as
> *optional* in HSFZ, and 5 of the 10 un-acked frames still received a response. So a
> missing ack is a strong empirical indicator **on this gateway** but is **not** a
> sound standalone failure trigger. Recommendation below uses it as an observability
> signal and an early-warning input, never as the sole basis for declaring failure.

---

## A. Retry — exact ISTA semantics

### A.1 There are two retry layers, and the C# one is off by default

**Layer 1 — native EDIABAS core.** `EDIABAS.INI:31` `RetryComm = 1`, documented at
`EDIABAS.INI:403-408` ("Repeat failed communication automatically (1x)"). The
mechanism lives in `ebas32.dll`/`ebas64.dll` + `XEnet32.dll`/`XEnet64.dll`, which are
**native PE, not IL — `ilspycmd` cannot read them**. What is retried at this layer,
whether the retry is per-telegram or per-`send_and_receive`, and any inter-attempt
delay are **NOT DETERMINABLE** from the shipped binaries. I am not guessing at it.
Match observable behaviour instead.

**Layer 2 — ISTA C# (`ECUKom.apiJob`).** Fully readable
(`ista-decompile/BMW.Rheingold.VehicleCommunication.ECUKom.decompiled.cs:1422-1447`):

```csharp
IEcuJob ecuJob = apiJob(ecu, jobName, param, resultFilter, fastaprotocoller, callerMember);
if (ecuJob.JobErrorCode == 98) { return ecuJob; }          // :1435  never retry
ushort num = 1;
while (num < retries && !ecuJob.IsDone())                  // :1440
{
    SleepUtility.ThreadSleep(millisecondsTimeout, ...);    // :1442
    ecuJob = apiJob(ecu, jobName, param, resultFilter, fastaprotocoller, callerMember);
    num++;
}
```

Load-bearing details:

- **`num` starts at 1**, so `retries = 1` makes `1 < 1` false → **zero C# retries**.
  The default is `RetryCount => ConfigSettings.getConfigint("BMW.Rheingold.Diagnostics.VehicleIdent.RetryCount", 1)`
  (`decompiled/VehicleIdent.cs:158`, const `DefaultRetryCount = 1` at `:76`).
  **So in a stock ISTA install the C# retry loop never executes.** All the real retry
  behaviour is the native `RetryComm=1`.
- **Delay between C# attempts is `millisecondsTimeout`, which is 0** on the common
  path — the `:1409` overload calls `:1419` passing a literal `0`. Some call sites
  pass a value explicitly (e.g. `ReadPwfState` uses `3, 500` —
  `VehicleIdent.cs:5658-5681`), but the default is no delay.
- **Retry trigger is `!IsDone()`.** `IsDone()` is `JobErrorCode == 0 && JobResult != null
  && JobResult.Count > 0` (`scratchpad/ECUJob.cs:60-75`). So a retry fires on an
  EDIABAS-level error (which includes `IFH-0009: NO RESPONSE FROM CONTROLUNIT`,
  `ECUKom.decompiled.cs:1557`) **or** a job that returned no results. It does **not**
  fire on a job that completed with results, however unwelcome those results.

### A.2 What error 98 and `ERROR_ECU_NACK` actually mean

The brief's framing ("ISTA treats error 98 and `ERROR_ECU_NACK` as SUCCESS") is
correct as to the branches, but the interpretation needs correcting:

**Error 98 = `EDIABAS_SYS_0008` (`scratchpad/apiNET464.cs:516`) = `SYS-0008: JOB NOT
FOUND`** — extracted from the native core's string table:
`strings -a ebas32.dll | rg 'SYS-000[0-9]'` → line 1140 `SYS-0008: JOB NOT FOUND`.

This is **not a wire condition at all.** It means the ECU's SGBD does not define the
requested job. `VehicleIdent.cs:2981-2985` sets `mECU.FS_SUCCESSFULLY = true` on
error 98 because there is no fault memory to read on that variant — nothing failed.

> **klartext mapping:** there is **no UDS/wire equivalent**, and klartext will never
> observe it on the wire. The klartext analogue is a *catalog* condition — "this
> variant has no such job / no fault-memory job" — and the parity behaviour is to
> report that ECU as **successfully read with nothing to report**, not as an error.
> This also means error 98 is irrelevant to the dropout problem. Do not model it as a
> transport outcome.

**`ERROR_ECU_NACK`** is a `JOB_STATUS` *string result*, tested via
`IsJobState("ERROR_ECU_NACK")` (`VehicleIdent.cs:2827-2830`, `:2968-2971`).

I could **not** determine which wire condition produces it. Explicit negatives:
it does not appear in the native core's string table (`strings -a ebas32.dll`), and
decoding `d72n47a0.prg / FS_LESEN` with klartext's own `klartext_best::decode_job`
(1 472 ops) surfaced no `ERROR_ECU_NACK`, `JOB_STATUS` or `OKAY` literal.

What *can* be stated rigorously, from `ECUJob.cs:103-109`:

```csharp
public bool IsJobState(string state)
{
    if (IsDone()) { return getResultsAs<string>("JOB_STATUS") == state; }
    return false;
}
```

`IsJobState` requires `IsDone()`, which requires `JobErrorCode == 0` **and**
`JobResult.Count > 0`. Therefore `ERROR_ECU_NACK` can only occur on a job that
completed cleanly at the EDIABAS level with results — **the ECU answered**. A timeout
produces `JobErrorCode = 19 / IFH-0009` instead and can never reach this branch.

> **klartext mapping:** `ERROR_ECU_NACK` corresponds to **a UDS negative response
> (`0x7F …`) that the SGBD classified as a NACK** — i.e. klartext's
> `ClientError::Negative { sid, nrc }`. It is **not** a timeout and **not** a
> transport error. **Which NRC(s)** map to it is **UNDETERMINED** — I could not read
> the classification and will not invent it.

### A.3 The ident-level retry and `DoECUIdentDeepAwake`

Separate from `ECUKom`, `doMissingECUIdent` has its own loop
(`decompiled/VehicleIdent.cs:869-933`):

```csharp
for (int i = 0; i < retry + 1; i++)                     // :886  retry=1 → 2 attempts
{
    foreach (string text in array2)                      // ECU_GRUPPE split on '|'
    {
        ecuJob = ecuKom.apiJob(text, "IDENT", string.Empty, string.Empty);   // :905
        if (ecuJob.IsOkay()) { i = retry; DoAfterIdentProcessing(...); break; }  // :907-912
        HandleSpecificMotorcycleECU_GRUPPE(mECU);
        if (i > 0) { DoECUIdentDeepAwake(mECU, ..., tryReanimation, ...); }  // :915-918
    }
}
```

Note the deep awake fires **only when `i > 0`**, i.e. after the *second* failure. With
the default `retry = 1` the loop ends immediately afterwards, so the awake's benefit
lands on a **later** phase, not this loop — `DoECUIdentDeepAwake` ends by setting
`mECU.IDENT_SUCCESSFULLY = false` (`:1135`) and later phases
(`ForcePhysicalIdentOnUnindentified`, `:966-985`) re-attempt.

**What the deep awake does on the wire** — `VehicleIdent.cs:1344-1350`, the whole
method body for a BN2020 (F-series) car:

```csharp
ecuAwake = ecuKom.apiJob(sgbd,   "NORMALER_DATENVERKEHR",       "ja;nein",           "");
ecuAwake = ecuKom.apiJob("D_EXX","ENERGIESPARMODE_FUNKTIONAL",  "0x{addr};aus;aus;aus","");
ecuAwake = ecuKom.apiJob("G_FXX","ENERGIESPARMODE_FUNKTIONAL",  "0x{addr};aus",      "");
ecuAwake = ecuKom.apiJob(sgbd,   "STEUERN_BETRIEBSMODE",        "0x00",              "");
```

Dispatch into this method is guarded (`:1120-1134`): the BN2000 functional-ident branch
does not apply to F-series; `DoMOSTBruteForceIdent` only if `mECU.BUS == MOST`; the
`DeactivateAllTransportModes_BN2000_BN2020` call applies to BN2020 and is the relevant
one for both the F20 and F25. It is further gated on config
`BMW.Rheingold.Diagnostics.VehicleIdent.DoBruteForceIdent` (default `true`) and
`!mECU.COMMUNICATION_SUCCESSFULLY`.

**Two things follow, and they matter more than the awake itself:**

1. **These are `STEUERN_*` jobs — actuation, not reads.** Under klartext's tier ladder
   they are **service writes**, requiring `confirm=true` + preconditions. A "wake the
   sleepy ECU" helper cannot be added to a read path without breaching the blast-radius
   rule. Do not implement it as part of the dropout fix.
2. **The wire bytes are not available in this data set.** `ENERGIESPARMODE_FUNKTIONAL`
   is defined on the group SGBDs `D_EXX` / `G_FXX`; the shipped `g_fxx.grp` parses to
   only two jobs (`INITIALISIERUNG`, `IDENTIFIKATION`) via `klartext_sgbd::Prg`, and a
   scan of all 1 832 SGBDs in `data/Testmodule(1)/Ecu/` found **zero** jobs named
   `*ENERGIESPARMODE_FUNKTIONAL* ` (1 540 hits for `ENERGIESPARMODE`, 403 for
   `NORMALER_DATENVERKEHR`, 533 for `STEUERN_BETRIEBSMODE`, none for the `_FUNKTIONAL`
   variant). **The deep-awake UDS payloads are NOT DETERMINED.** Do not synthesise them.

**Conclusion for P1.1: the deep awake is *not* the answer to the sleepy-ECU problem
here, because there is no sleepy-ECU problem.** The evidence in §0 shows the ECUs were
awake and answering throughout. Implementing reanimation would add gated actuation to
fix a fault that serialisation removes for free.

### A.4 klartext today

- **Retry: none.** `rg 'retry|Retry|attempts|backoff' crates/client/src crates/hsfz/src`
  returns only doc-comment prose (`crates/client/src/error.rs:39,41`,
  `crates/client/src/client.rs:323`). One timeout is a permanent failure.
- **`0x78`: correctly handled** — see §0 item 2.

---

## B. Serialisation

### B.1 ISTA has no concurrency in vehicle communication — and no lock either

`ECUKom` holds a single blocking EDIABAS handle: `private API api;`
(`ECUKom.decompiled.cs:78`), and every job funnels through `api.apiJob(...)`
(`:1594`) followed by a busy-wait (`:1597` `SleepUtility.ThreadSleep(2, ...)`).

**Explicit negative, and it is an interesting one:** `rg 'lock \(|Monitor\.Enter|Semaphore|MethodImplOptions\.Synchronized'`
over the entire `ECUKom` decompile returns **zero matches**. There is no mutex, no
semaphore, no synchronised method. So the serialisation is **an implementation
artefact of a single blocking handle plus synchronous call style — not an enforced
protocol lock.** ISTA is sequential because it never tries not to be, not because
something stops it.

That distinction matters for klartext: it means ISTA's design gives us **no evidence
either way** about whether the gateway *requires* single-outstanding. So do not cite
ISTA as proof of a protocol constraint.

### B.2 What the gateway actually imposes — empirical, from the owner's capture

`docs/protocol-reference.md` does not state a single-outstanding constraint; it says
the ZGW "routes the UDS payload onto the correct internal CAN/FlexRay bus and relays
the response back" (`:224`) and lists the `0x02` ack as optional (`:309`).
`crates/client/src/session.rs:26-28` already flags this as unverified:
"[verify live]: whether the ZGW tolerates *interleaved* requests to different targets;
the pcap is lockstep".

**This capture now answers that open question: it does not tolerate it well.** Per §0 —
0 % loss at depth 0–1 over 388 requests, 27–100 % loss at depth ≥ 3, with 5 of the 10
losses refused at the HSFZ admission layer (no ack). Whether the true limit is exactly
1, or 2, is not established by this data — the capture only contains depth-0/1 and
depth-≥3 populations, with **no depth-2 samples at all**. Depth 2 is untested.

### B.3 Cost

The owner has already ruled "match ISTA: serialise", so this is sizing, not deciding:

- **Observed `19 02` round-trip:** min 8 ms, median 162 ms, p90 394 ms, max 437 ms (n=29).
- **Sequential 32-ECU fault sweep at observed latencies: ≈ 4.8 s.**
- **Observed sequential throughput (the early ident sweep): 388 requests in 6.41 s = 16.5 ms/request, zero loss.**
- **ISTA's pathological worst case:** `EDIABAS.INI:144` `[XEthernet] TimeoutFunction = 1200`
  → 32 × 1.2 s = **38.4 s** if every ECU times out. That is the bound, not the expectation.

Note `TimeoutFunction = 1200` is in the **`[XEthernet]`** section — the ENET/HSFZ
interface klartext actually uses (`EDIABAS.INI:10` `Interface = ENET`). The `[TCP]`
section's `TimeoutFunction = 10000` (`:56`) is a different transport and does not
apply. klartext's current 5 000 ms `read_timeout` is **more than 4× ISTA's ENET
function timeout** — a parity divergence in its own right (see change 4).

---

## C. Reconnect and VIN re-validation

Assembly: `RheingoldSessionController.dll`, type
`BMW.Rheingold.RheingoldSessionController.Logic`. Decompiles saved alongside this file.

**Anchor correction:** `OpenConnectionLossPopupToAskUserForReconnection` **does not
exist** in any of the 147 binaries. The real symbol is
`OpenConnectionLossPopupToAskUserForReconnectOrBreakConnectionAsync`, in a *different*
type — `SessionLogic.cs:1043`, not `Logic`.

### C.1 Detection — ISTA has NO active connection-loss polling on ENET

There are two mechanisms, and **the watchdog does not apply to our transport**:

- **Watchdog — ICOM only.** `SessionLogic.cs:182` creates a 5 s `Timer`, but
  `Configure(VCIDevice)` (`:688-724`) starts it **only** in `case VCIDeviceType.ICOM:`
  (`:701`). `VCIDeviceType.ENET` is a distinct enum member (`Logic.cs:3921`) absent from
  that switch → falls to `default:` → `vciDeviceConnectionWatchDogTimer.Stop()`
  (`:719-720`). Corroborating: the heartbeat it consumes (`KlVoltageLastMessageTime`) is
  written only under an `ICOM.Equals(x.VCIType)` filter (`:238`, `:266`), and clamp
  voltage for ENET is hardcoded `"--"` (`:723-724`).
- **Reactive, job-driven — this is the ENET path.** `EcuKomServiceDlgImpl.cs:457`:

  ```csharp
  if (text2.Equals("NET-0014: CONNECTION ABORTED") || text2.Equals("NET-0009: TIMEOUT"))
  {
      sessionLogic.StopWatchDogTimer();
      sessionLogic.ShowVciLossConnectionInEcuKomServiceDlg();
      bool flag3 = ShowQuestionDialog(list, list2);          // "#VCILoss.ResendJob"
      if (flag3) { diagnosticDeviceResult = eDIABASAdapter.Execute(inParameters); }  // resend
      else       { AbortTestModule(); }
      finally    { sessionLogic.StartWatchDogTimer(); }
  }
  ```

  The link-gone vs ECU-silent distinction is explicit: `NET-0014` / `NET-0009` = link
  gone; `IFH-0009: NO RESPONSE FROM CONTROLUNIT` (`ECUKom.decompiled.cs:1557`, `:1974`)
  = ECU silent, an ordinary job failure with no connection-loss path.

Detection is therefore "whatever the socket surfaces on the next job", tightened by
`EDIABAS.INI [XEthernet]:154-160`: `EnableTcpKeepAlive = 1`, `TcpKeepAliveTime = 1`,
`TcpKeepAliveInterval = 1000`, `TcpKeepAliveRetries = 0`.
**How XEnet maps those onto socket options is NOT DETERMINABLE** (native PE); the units
(`Time = 1` s or ms) are unverified.

**Explicit negative:** `NetworkChange.NetworkAddressChanged` is subscribed
(`SessionLogic.cs:746`) but its callback only logs to FASTA (`:679-686`). No TCP/NIC
event drives reconnect.

### C.2 The VIN re-validation read

Chain: `CompareSessionVinToEcuJobVin` (`Logic.cs:5981`) →
`GetVinFromVehicleWithoutICOMAndUserInput` (`:5984`) → `GetVinViaRplusOrEnet`
(`VehicleIdent.cs:8552-8555`) → `GetVin17` (`:8757`) → `GetVin17GroupCars` (`:408`) →
`ReadVinForGroupCars` (`:421`).

For **BN2020** (both cars) the ladder is `DiagnosticsBusinessData.cs:1889-1891,
1903-1906` — first non-empty wins:

| SGBD | Job | Set | Result |
|---|---|---|---|
| `G_ZGW` | `STATUS_VIN_LESEN` | 1 | `STAT_VIN` |
| `G_CAS` | `STATUS_FAHRGESTELLNUMMER` | 1 | `STAT_FGNR17_WERT` |
| `G_FRM` | `STATUS_VCM_VIN` | 1 | `STAT_VIN_EINH` |

`ReadVinFromEcus` (`:1974-2001`) rejects the sentinel `"00000000000000000"` and falls
through to the next rung. The whole ladder runs once by default
(`VehicleIdent.cs:420` `for (int i = 1; i <= retries; i++)`, `retries = RetryCount = 1`).

**Job → UDS resolved**, by disassembling the shipped `.prg` with klartext's own tooling:
`zgw_01.prg / STATUS_VIN_LESEN` contains `move S1, "[22 F1 90]"` then `xsend S3, S1`;
`cas4_2.prg / STATUS_FAHRGESTELLNUMMER` (the F25's CAS4) is identical.

> **klartext is already byte-identical to ISTA on this read (`22 F1 90`).** The only
> divergence is that ISTA walks a **three-ECU fallback ladder** (ZGW → CAS → FRM) where
> klartext reads one address.

### C.3 What is compared

`Logic.cs:5981-6001` — **full 17-char, ordinal, case-SENSITIVE, no trimming**:

```csharp
if (!string.IsNullOrEmpty(vin))
{
    if (vin.Length == 7)
        new SVMDProcessorImpl(...).ResolveVIN7ToVIN17(vin, ref vin, Services);  // backend call
    if (vin.Equals(vecInfo.VIN17)) boolResultObject.Result = true;
    else                            boolResultObject.ErrorCode = "VehicleVinNotMatch";
}
```

A 7-char read is expanded to 17 via a backend call *before* comparison; there is no
short-VIN comparison on this path.

**A second, different comparator exists and must not be confused with it.**
`VehicleIdent.DoVehicleCheck` (`VehicleIdent.cs:6695`, `:6706`) is
**case-INsensitive with a VIN7 fallback**:

```csharp
flag = string.Compare(vin17ToCheck, vinViaRplusOrEnet, StringComparison.OrdinalIgnoreCase) == 0;
if (!flag && vinViaRplusOrEnet.Length == 17)
    flag = string.Compare(vin17ToCheck, 10, vinViaRplusOrEnet, 10, 7, StringComparison.OrdinalIgnoreCase) == 0;
```

That one is **not** on the connection-loss path (which calls `HandleVCI` directly;
`CheckContinueVecInfo` merely assigns `vecInfo.VCI = device` and returns,
`Logic.cs:4127-4136`). **The two are not interchangeable — pick the connection-loss one
(strict, full-17, case-sensitive) for reconnect.**

Also `Diagnostics.CheckCrossReconnectAllowed()` (`Diagnostics.cs:62-86`): a license
sub-package `CrossReconnectAllowed` with `PackageRule == "true"` makes `DoVehicleCheck`
return true **regardless of VIN mismatch** (`VehicleIdent.cs:6699-6703`). License-gated;
not reachable in a normal workshop config. Do not port it.

### C.4 Mismatch, and the distinct "unreadable" case

**Mismatch = hard abort + full disconnect.** `VciConnLossVM.cs:40-53`:

```csharp
if (r.Result) { NotifyResponse(..., InteractionButton.Continue); }
else if (!string.IsNullOrEmpty(r.ErrorCode) && r.ErrorCode.Equals("VehicleVinNotMatch"))
{
    NotifyResponse(..., InteractionButton.Abort);
    await logic.CurrentOperation.DisconnectDeviceOverLossConnection(logic.VecInfo.VCI);
}
```

`DisconnectDeviceOverLossConnection` (`IstaOperationActionImpl.cs:240-248`) stops a
running therapy plan, disconnects FASTA, then `DisconnectDevice` (`:299-306`) calls
`logic.DisconnectEcuKom()`, `logic.DisconnectVCI(...)` and **replaces the VCI with a
dummy** `new VCIDevice(VCIDeviceType.INFOSESSION, "InfoSession", "127.0.0.1")`. Not
dismissable, not a new session. (Note the branch compares the *string*, not the enum.)

**Unreadable VIN is a third outcome — neither match nor mismatch.** `BoolResultObject`
has no field initialisers, so an unread VIN yields `Result=false, ErrorCode=null`;
every assignment in `CompareSessionVinToEcuJobVin` sits inside
`if (!string.IsNullOrEmpty(vin))`. Traced through `CmdReconnectExecute`: first branch
skipped (`Result` false), second skipped (`ErrorCode` empty) → **no `NotifyResponse` is
ever sent**, the `await RegisterAsync(model)` (`SessionLogic.cs:1049`) stays pending and
the popup stays open. The user sees the generic
`#ConnectionLossUnableToConnectToIcom` (`IstaOperationActionImpl.cs:994-1013`) and may
press Reconnect again. **Retry is manual and unbounded; no counter, no backoff.**

Two hard gates precede any VIN read (`IstaOperationActionImpl.cs:973-989`):
`vecInfo.VehicleIdentAlreadyDone` must be true, and the device must not be
`UNKNOWN`/`IMIB`.

### C.5 Ignition / clamp — not checked on this path

**Explicit negative.** `ClampSwitchVehicle` is a test-module ignition-cycling helper,
not reconnect infrastructure: `ClampSwitchVehicle` and `DoClampSwitch` each appear in
exactly **one** assembly (`RheingoldDiagnostics.dll`) — the definition only, with no
cross-assembly caller. It self-gates on `BNType == BN2020` plus config
`BMW.Rheingold.AutomaticClampSwitchVehicle.Enabled` (`ClampSwitchVehicle.cs:65-79`).

Searching all DLLs/EXEs for `STATUS_KLEMMEN`, `STATUS_KL15`, `STATUS_PWF`,
`STATUS_SPANNUNG` as job-name strings returns **nothing**. What exists instead:
clamp voltage is a **VCI device property**, not a job (`ClampSwitchVehicle.cs:246` →
`VCIDevice.cs:1600`; thresholds 7000/9000 mV at `:20-22`; polled at 1 Hz at `:252`) —
**and for ENET it is always `"--"` → null**. `ReadPwfState` (`VehicleIdent.cs:5658-5681`)
early-returns unless `HasPad(bdc)`. A clamp **write** exists but is ICOM/PTT-only:
`ecuKom.apiJob("G_CAS", "STEUERN_KLEMMEN", "KL15_ein", "")` (`VehicleIdent.cs:5079-5093`).

Decisively, the shipped config disables EDIABAS clamp handling entirely —
`EDIABAS.INI:27-29` `UbattHandling = 0`, `IgnitionHandling = 0`, `ClampHandling = 0`,
documented at `:386-399` ("0 = Ignition ON/OFF: No EDIABAS error", "0 = no automatic
clamp check with send_and_receive"). **On ENET, ignition state gates nothing and raises
no error** — consistent with session 1's gateway FIN+RST at ignition-off, which ISTA
would see as `NET-0014`, not as a clamp condition.

### C.6 Reconnect mechanics

**Full teardown and re-init; the handle is not reused.** `ECUKom.Refresh`
(`ECUKom.decompiled.cs:353-365`) is `End(); InitVCI(VCI, isDoIP);` where `End()`
(`:297-311`) sets `apiSetConfig("EDIABASUnload", "1")` then `apiEnd()` — a full unload,
not a socket close.

User-facing flow: loss detected → `ShowConnectionLossMessage` (`SessionLogic.cs:608-621`,
suppressed while a TAL executes) → `OpenConnectionLossPopup` (`:1021-1030`) picks the
close-only dialog when `!IsVehicleTestDone`, else Reconnect/Abort → Reconnect
**re-opens the Connection Manager for the user to re-pick a device**
(`IstaOperationActionImpl.cs:955-971`) → `PerformVinCheckOverConnectionLossPopup`
(`:169-184`) → `Logic.CheckVinOverConnectionLossPopup` (`:3698-3723`) → routed per §C.4.

**No backoff, no retry count, no automatic reconnect anywhere on this path.** Every
retry is a human button press.

---

## D. Mapping to klartext — concrete change list

Constraints honoured: async tokio; surfaces are the MCP server and the future mobile
app (no CLI); `thiserror` in libraries; `klartext-best` must not gain a dependency on
`klartext-client`.

### Change 1 — Serialise the whole-car sweep (P1.2) — **fixes the dropouts**

**File:** `crates/client/src/scan.rs:86-113` (`scan_faults`), and the caller
`mcp/src/server.rs:1417` + the default at `mcp/src/config.rs:68`.

`scan_faults` currently ends `.buffer_unordered(concurrency.max(1))`. The minimal,
lowest-risk change is to **make sequential the default** rather than to rip out the
parameter: change `mcp/src/config.rs:68` `default_value_t = 8` → `default_value_t = 1`.
That alone restores the depth-0 behaviour that measured 0 % loss over 388 requests.

Stronger option, and the one I recommend: **remove the `concurrency` parameter from
`scan_faults` entirely** and make the loop a plain sequential `for`. Rationale — a
tunable whose only safe value is 1 is not a tunable, and leaving it exposed invites the
same 31 % data loss to be re-enabled by a flag. This matches the anti-overengineering
rule ("hardcode values with one legitimate setting"). Keep `--scan-concurrency` as a
deprecated no-op only if the owner wants CLI/MCP arg stability; otherwise delete it.

Note the concurrency machinery is already inconsistent with the codebase's own stated
posture: `crates/client/src/scan.rs:9-11` says reads "fan out concurrently" while
`session.rs:26-28` flags the very same behaviour as `[verify live]`. This change
resolves that contradiction in the direction the capture now supports.

**Do not** serialise at the `Session` layer (a global mutex around `request`). The
per-target demux in `session.rs` is correct and useful — `run_job` and future
procedures legitimately interleave. Serialise **the sweep**, which is the only place
that fans out.

### Change 2 — Add a retry at the session seam (P1.1)

**File:** `crates/client/src/session.rs`, inside `request_with_timeout` (`:195-256`).

This is the right seam because every read path in the workspace bottoms out here
(`DiagnosticClient::read_all_dtcs`, `read_data`, `read_did`, `read_info_memory`, the
`SessionBridge` the BEST/2 VM runs over), so one change covers them all uniformly
instead of being bolted onto `scan_faults`.

Shape:

```rust
/// ISTA parity: EDIABAS.INI:31 `RetryComm = 1` — one automatic repeat of a failed
/// exchange. Applies ONLY to retry-safe (idempotent) services; see `is_retry_safe`.
const RETRY_COMM: u32 = 1;
```

- **Retry only on timeout / transport error**, i.e. `ClientError::Hsfz(ReadTimeout)`.
  **Never** retry a `ClientError::Negative` — per §A.2 an NRC is ISTA's
  `ERROR_ECU_NACK`-shaped outcome, a job that *completed*, and `!IsDone()` is false for
  it, so ISTA does not retry it either. Retrying an NRC would be a divergence.
- **No inter-attempt delay** — matches the C# layer's `millisecondsTimeout = 0`
  (`ECUKom.decompiled.cs:1442` via the `:1419` overload). Do not invent a backoff.
- **Gate on idempotency.** Add to `crates/uds` (the crate both `client` and `best`
  already depend on — `crates/client/Cargo.toml:13`, `crates/best/Cargo.toml:13`):

  ```rust
  /// Whether an automatic retry of this service is safe (idempotent).
  pub fn is_retry_safe(sid: u8) -> bool {
      matches!(sid, 0x10 | 0x3E | 0x22 | 0x2C | 0x19)
  }
  ```

  This set is deliberately identical to `klartext_best::gate::SidClass::Pass`
  (`crates/best/src/gate.rs:87-100`) but is a **separate predicate on a separate axis** —
  retry-safety is not blast radius. Document that: do not merge them, and do not make
  `client` depend on `best` to reuse `classify`.

> **DIVERGENCE FROM ISTA — needs the owner's explicit ratification before it ships.**
> ISTA's retry is at the *job* level and would repeat the whole job, including a write
> such as `FS_LOESCHEN` (`0x14`). klartext's proposed retry deliberately **excludes
> every write**, so a failed clear is surfaced rather than silently re-sent. I believe
> this is correct — silently repeating an actuation contradicts the tier ladder's
> "confirm relayed from the human" rule, and a `0x14` retry after an ambiguous timeout
> could clear a fault the human never saw. But per the 1:1 mandate it is a divergence
> and must be written into CLAUDE.md with the owner's agreement, not chosen silently.

### Change 3 — Observe the HSFZ ack (P1.2 support, diagnostic only)

**File:** `crates/client/src/session.rs:295-298` (`route_frame`).

Today every non-`DIAGNOSTIC` frame is dropped unread. Given §0 item 3 — 99.34 % ack
rate, p95 1.56 ms, and 5 of 10 losses un-acked — the ack carries real signal.

Recommended shape: record acks per target and emit `tracing::debug!` when a request
completes with no ack observed. **Do not** make a missing ack fail the request: the
protocol calls it optional (`docs/protocol-reference.md:309`) and 5 of 10 un-acked
frames still got responses. It is an observability and early-warning input only.

This is cheap and makes the next on-car session self-diagnosing without a pcap.

### Change 4 — Align the read timeout with ISTA's ENET value

**File:** `crates/client/src/client.rs:62`, currently
`read_timeout: Duration::from_millis(P2_STAR_SERVER_MAX_DEFAULT_MS)` = **5 000 ms**
(`crates/uds/src/lib.rs:128`).

ISTA's ENET function timeout is **1 200 ms** (`EDIABAS.INI:144`, `[XEthernet]`). The
observed `19 02` max round-trip was 437 ms, so 1 200 ms is comfortably above real
latencies and cuts the cost of a genuinely-silent ECU by 4×.

Keep `P2_STAR_SERVER_MAX_DEFAULT_MS = 5000` for the **`0x78` re-arm budget** — that is
a different timer (ISO 14229-2 P2*), and the capture confirms real `0x78`s occur. Only
the *initial* per-request timeout should move to 1 200 ms.

> Flag: this is a behavioural change to every read. It is ISTA parity, but it should
> land with change 1 so the on-car test measures both together.

### Change 5 — VIN re-validation (P1.3)

The owner already considers this worth porting regardless of reconnect triggering.

**Files:** `crates/client/src/client.rs` (a `verify_vin(&self, expected: &str)`),
plus the connect path that records the session VIN.

- **Read:** `22 F1 90` — klartext already matches ISTA byte-for-byte (§C.2).
- **Comparison:** full 17-char, **case-sensitive**, ordinal, no trimming — the
  connection-loss comparator (`Logic.cs:5994`), *not* the case-insensitive
  `DoVehicleCheck` one. Cite both in the doc comment so the choice is not "fixed" later
  by someone who found the other.
- **Three distinct outcomes**, mirroring §C.4 — this is the part most likely to be got
  wrong by collapsing it into a bool:

  ```rust
  pub enum VinCheck {
      Match,
      Mismatch { expected: String, found: String },  // ISTA: hard abort + disconnect
      Unreadable,                                    // ISTA: neither; prompt stays open
  }
  ```

  `Unreadable` must **not** be reported as a mismatch. ISTA distinguishes them
  explicitly and the failure modes are different (a silent ZGW is not a swapped car).
- **ECU ladder:** ISTA tries ZGW → CAS → FRM. klartext reads one address. Porting the
  ladder is a genuine parity gap; it is small (three addresses, first non-empty wins,
  reject the `"00000000000000000"` sentinel — `DiagnosticsBusinessData.cs:1974-2001`).
  Recommend porting it with the VIN check rather than as a separate task.
- **Do not** port `CrossReconnectAllowed` (§C.3).

### Change 6 — Reconnect (P1.3) — deliberately minimal

**File:** `crates/client/src/session.rs:148-160` (reader task; today a closed connection
fails every waiter with `ClientError::ConnectionClosed` and nothing recovers).

**ISTA does not auto-reconnect** (§C.6): no backoff, no retry ceiling, a human button
press each time, and a full EDIABAS unload + re-init rather than handle reuse. So
parity here means **surfacing a typed, actionable "connection lost" state and requiring
an explicit re-connect call** — not building a resilience layer ISTA does not have.

Concretely: keep the current fail-fast behaviour, and add a distinct error/state the
MCP layer can report so the agent tells the human "the car dropped the link (ignition
off?) — reconnect and I will re-verify the VIN". On reconnect, run change 5 **before**
any further read; on `Mismatch`, tear the session down (ISTA's `DisconnectDevice`
equivalent) rather than continuing.

Do **not** port the ICOM watchdog — it is stopped for ENET (§C.1) and would be a
klartext invention. Do **not** add clamp/ignition gating — ISTA has none on ENET (§C.5).
`EnableTcpKeepAlive` is worth mirroring on the socket, but note the INI→socket-option
mapping is **not determinable** (native PE), so treat values as a starting point rather
than parity.

---

## E. Ordering, risk, and the on-car test

### E.1 Ordering

1. **Change 1 (serialise) — first, alone.** It is the fix. One-line default flip or a
   small loop rewrite, no new abstractions, and the capture already predicts the
   outcome (0 % loss at depth 0–1 over 388 requests).
2. **Change 4 (timeout 1 200 ms)** — land with 1 so a single on-car run measures both.
3. **Change 3 (ack observability)** — cheap, makes run 2 self-diagnosing.
4. **Change 2 (retry)** — *after* 1 is confirmed. Retry is belt-and-braces, not the fix;
   note that with 5 of 10 losses refused at the HSFZ admission layer and **zero late
   responses anywhere in the 202–215 s window**, retrying at depth 8 would likely have
   re-lost the same requests. Retry without serialisation would have masked the real
   cause. Requires the owner's ratification of the §D-change-2 divergence first.
5. **Changes 5 + 6 (VIN + reconnect)** — independent of the dropout fix; sequence by
   owner preference. Change 5 has standalone safety value (an agent reasoning about the
   wrong car) and does not depend on 1–4.

**Risk note:** changes 1 and 4 alter timing on every read path. Both are cheap to
revert. Change 2 is the only one that changes what klartext puts on the wire in a
failure case, which is why it is gated behind an explicit owner ruling.

### E.2 The cheapest confirming experiment — one run, three hypotheses separated

I cannot reach the car and make **no claim that any of this is hardware-verified**. The
following is the minimal manual procedure that discriminates *contention* from *sleepy
ECU* from *0x78 mishandling* in a single sitting, with a pcap as the arbiter.

**Setup.** Ignition on (or at least terminal 15 held as in session 1). Start the capture
*before* connecting so the HSFZ handshake is included:

```
tcpdump -i <enet-if> -w ~/klartext-p1-test.pcapng 'tcp port 6801 or tcp port 6811'
```

Run everything through the MCP server with `RUST_LOG=klartext_client=trace` so the TX/RX
trace (`session.rs:230`, `:303`) lands in stderr alongside the pcap.

**Step 1 — reproduce the failure (control).** With today's default (`scan_concurrency = 8`),
run one whole-car fault scan. Record which addresses fail.
*Expected:* ~10 of 32 fail, and the failing set overlaps `06 08 18 19 29 2c 30 5d 67 72`.

**Step 2 — the discriminator.** Immediately, without cycling ignition, re-run the same
scan with `--scan-concurrency 1`.
- **All 32 answer → CONTENTION confirmed.** This is the predicted outcome and it also
  rules out sleepy ECUs, because nothing was woken between steps 1 and 2.
- **The same ECUs still fail → NOT contention.** Then it is genuinely ECU-side, and
  step 3 matters.

**Step 3 — sleepy-ECU control (only if step 2 still fails).** Address one failing ECU
individually (a single `read_faults` on that address). If it answers alone but not in a
sequential sweep, the problem is sweep *pacing* (inter-request gap), not depth — try an
inter-request delay before considering anything like ISTA's deep awake. Note session 1
already provides this control for free: every one of the ten answered 12–15 times
individually.

**Step 4 — `0x78` check (from the pcap, no extra car time).**
```
tshark -r ~/klartext-p1-test.pcapng -Y 'tcp.port==6801 && tcp.len>0' \
  -T fields -e frame.time_relative -e tcp.payload | rg '7f..78'
```
Any `7F xx 78` should be followed by the real response on the same target, and the read
should have succeeded. If a `0x78` is followed by a klartext timeout, *then* the handler
is implicated — but §0 item 2 makes that unlikely.

**Step 5 — ack check (the sharpest single signal).** Re-run the analysis in
`scratchpad/ack.py` against the new pcap. For each un-answered request, ask whether an
HSFZ control-`0x02` ack arrived. In step 1 the answer should be "no ack" for roughly
half the failures. In step 2 there should be **no un-acked frames at all**. This is the
cleanest confirmation that the gateway's admission control — not the ECUs — was
dropping the requests.

**Total car time: under five minutes** (two 32-ECU scans, ~5 s and ~10 s of actual
traffic). Everything else is offline pcap analysis.

---

## F. Where ISTA's logic proved unreadable

Stated explicitly, as the mandate requires — these were **not** guessed at:

- **`XEnet32.dll` / `XEnet64.dll`** — native PE, not IL. Owns the `RetryComm = 1`
  mechanism, TesterPresent cadence, NRC `0x78` retry, and the
  `EnableTcpKeepAlive`/`TcpKeepAliveTime`/`Interval`/`Retries` → socket-option mapping
  (including whether `Time = 1` is seconds or milliseconds). **What EDIABAS actually
  retries, at which layer, and with what delay is NOT DETERMINABLE.**
- **`ebas32.dll` / `ebas64.dll`** — native PE. Only the error-code *string table* was
  recoverable (via `strings`), which is how `SYS-0008: JOB NOT FOUND` was pinned.
- **`ERROR_ECU_NACK` → wire condition** — the specific NRC(s) are **UNDETERMINED**. Not
  in the native string table; not recoverable from `d72n47a0.prg / FS_LESEN` via
  `klartext_best::decode_job` (1 472 ops, no matching literal). Only the structural
  inference in §A.2 (the ECU answered; it is a negative response, not a timeout) is
  supported by evidence.
- **`ENERGIESPARMODE_FUNKTIONAL` UDS payload** — the job is not present in any of the
  1 832 SGBDs in `data/Testmodule(1)/Ecu/`; `g_fxx.grp` parses to two jobs only. The
  deep-awake wire bytes are **NOT DETERMINED**.
- **Depth-2 concurrency behaviour** — the capture contains only depth-0/1 and depth-≥3
  populations. Whether the gateway's true limit is 1 or 2 outstanding requests is
  **not established**.
- Not touched, and not needed on this path: the KMM rules engine (interface only) and
  the PSdZ Sollverbauung matcher (external service).

---

## G. Reproduction artefacts

All under
`<scratchpad>/`:

| File | What it does |
|---|---|
| `sweep.py` | Parses HSFZ from the pcap; clusters `19 02` bursts; per-request response/latency; `0x78` census |
| `silent.py` | Proves the 10 "silent" ECUs answered elsewhere; checks for late responses |
| `corr.py` / `corr2.py` | Loss rate vs in-flight depth (`corr2` bounds lost-request occupancy at the 5 s timeout) |
| `ack.py` | Per-request HSFZ ack presence in the failing burst |
| `ackrate.py` | Whole-capture ack rate, latency distribution, un-acked census |
| `cost.py` | `19 02` round-trip distribution; sequential-sweep cost estimate |
| `ECUJob.cs` | `ilspycmd -t BMW.Rheingold.VehicleCommunication.ECUJob RheingoldVehicleCommunication.dll` |
| `apiNET464.cs` | Whole-DLL decompile; source of `EDIABAS_SYS_0008 = 98` |
| `grpdec/` | Throwaway cargo project using `klartext-sgbd` + `klartext-best` to scan/disassemble SGBDs |

`ilspycmd` is at `~/.dotnet/tools/ilspycmd` (not on `PATH`).
