# Standard OBD-II / SAE J1979 PID scaling (M5)

`klartext-semantic` scales a fixed set of **standard** OBD-II PIDs to engineering
units. These formulas are public (SAE J1979 / ISO 15031-5) — not BMW data. Nothing
here touches the EDIABAS SGBD; proprietary DID scaling stays out of scope until that
path exists (see [`sqlite-findings.md`](sqlite-findings.md)).

## How a PID is read

The existing read path uses ReadDataByIdentifier (UDS 0x22) with a 2-byte DID.
ISO 14229-1 mirrors the OBD-II service-0x01 PIDs into the **OBDDataIdentifier**
range `0xF400–0xF4FF`: DID `0xF4{PID}` returns the same datum as service-0x01 PID
`{PID}`. So engine RPM (PID `0x0C`) is read at DID `0xF40C`. `klartext-semantic`
maps any DID in that range to its PID (`pid::pid_for_did`) and scales it
(`pid::scale`). A DID outside `0xF4xx`, or a PID not in the table below, keeps the
existing named/raw behaviour — scaling never errors, it degrades to raw.

`A` is the first data byte, `B` the second (J1979 naming).

| PID | DID | Signal | Bytes | Formula | Unit | Range |
|-----|--------|--------|-------|---------|------|-------|
| 0x04 | 0xF404 | Calculated engine load | 1 | A × 100 / 255 | % | 0 – 100 |
| 0x05 | 0xF405 | Engine coolant temperature | 1 | A − 40 | °C | −40 – 215 |
| 0x0C | 0xF40C | Engine RPM | 2 | (256·A + B) / 4 | rpm | 0 – 16383.75 |
| 0x0D | 0xF40D | Vehicle speed | 1 | A | km/h | 0 – 255 |
| 0x0E | 0xF40E | Timing advance | 1 | A / 2 − 64 | ° (before TDC) | −64 – 63.5 |
| 0x0F | 0xF40F | Intake air temperature | 1 | A − 40 | °C | −40 – 215 |
| 0x10 | 0xF410 | MAF air flow rate | 2 | (256·A + B) / 100 | g/s | 0 – 655.35 |
| 0x11 | 0xF411 | Throttle position | 1 | A × 100 / 255 | % | 0 – 100 |
| 0x23 | 0xF423 | Fuel rail gauge pressure | 2 | (256·A + B) × 10 | kPa | 0 – 655350 |
| 0x46 | 0xF446 | Ambient air temperature | 1 | A − 40 | °C | −40 – 215 |

Worked vectors (also the unit tests in `crates/semantic/src/pid.rs`):

- Coolant `0xF405`, raw `7B` → 123 − 40 = **83 °C**.
- Engine RPM `0xF40C`, raw `0D 48` → (256·13 + 72) / 4 = **850 rpm**.
- Throttle `0xF411`, raw `FF` → 255 × 100 / 255 = **100 %**.

Fuel rail pressure uses PID `0x23` (fuel rail *gauge* pressure, ×10 kPa) — the
high-resolution rail signal for direct-injection engines like the F20's, not the
low-range manifold-relative PID `0x22`.

## Sources

- **SAE J1979** / **ISO 15031-5** — OBD-II diagnostic test modes; defines the
  service-0x01 PID set and the byte-to-value formulas above. Publicly tabulated
  (e.g. Wikipedia "OBD-II PIDs").
- **ISO 14229-1** — UDS; defines the `0xF400–0xF4FF` OBDDataIdentifier range that
  carries the OBD-II PIDs over service 0x22.

## Verifying against the car (manual)

The formulas are verified offline against known vectors. Physical confirmation is a
manual step: with the F20 awake, `klartext read-did F405` (coolant) or `F40C` (RPM)
and check the scaled value is plausible (coolant ≈ engine temp; RPM ≈ idle ~600–800
warm). The `0xF4xx` OBD mapping is an ISO standard, not BMW-confirmed for these
ECUs — if an ECU doesn't answer a `0xF4xx` DID, that signal isn't exposed there.
