# Field findings 2026-07-29 — F20 ZGW absent from the OBD Ethernet segment

**Session goal:** run `docs/on-car-test-protocol-2026-07-19.md` against the F20.
**Outcome:** blocked. A1–A7 and Phases B/C could not run — the gateway never appeared on
Ethernet. **A8 passed** (offline, no car needed). Everything klartext owns was proven healthy.

This document records the full elimination chain so a future session starts here rather than
re-deriving it.

---

## 1. Symptom

The F20's ZGW does not participate in the OBD Ethernet segment **at all** — not "has an address we
can't find", but emits zero frames. Meanwhile the car is fully functional: the engine starts and the
dash works, which requires the gateway routing between CAN buses. **The ZGW is alive on CAN and
absent on Ethernet.**

The same ENET cable and laptop reach the F25 X3's gateway instantly.

## 2. Timeline

| When | State |
|---|---|
| 2026-07-03 | This car answered on `169.254.90.33`. ENET worked. |
| 2026-07-27 | Session attempted. **0 packets captured**, `arp-scan /16` → **0 responded**. Dead wire. |
| 2026-07-28 | Owner: full 30-minute battery reset. |
| 2026-07-29 | Head unit now boots and is reachable. **ZGW still absent.** Partial recovery. |

The battery reset demonstrably changed things (dead wire → HU alive). It did not reach the ZGW.
Three further battery/ignition resets during the session produced no change.

## 3. Proven working — do not re-test these

| Property | Evidence |
|---|---|
| ENET cable, data pairs | full IP round-trip with the head unit through them |
| ENET cable, pin-8 activation leg | the **F25 works instantly with this same cable** — its ZGW needs the same activation |
| OBD pin 16 supply | owner measured; voltage and cable confirmed good |
| PHY / link | `carrier=1`, 100 Mb/s full duplex, `Link detected: yes` |
| Our probes egress eth0 | verified on the wire as broadcast to `ff:ff:ff:ff:ff:ff` |
| Our HSFZ ident frame is correct | the HU replies **`ICMP udp port 6811 unreachable`** — the car receives and processes our discovery datagram up its IP stack |
| ufw | `allow in on eth0` present; every `[UFW BLOCK]` is `IN=wlan0`; and `rx_packets` is counted in the **driver, below netfilter**, while tcpdump taps **AF_PACKET before netfilter** — a firewall cannot produce `rx=0` |
| DHCP service on the link | dnsmasq handed the HU a lease in seconds |

## 4. Proven absent

- `arp-scan` of all 65,536 of `169.254.0.0/16` → only the head unit.
- `arp-scan` of `192.168.0.0/16`, `172.16.0.0/16`, `10.0.0.0/16` (a ZGW on a static IP) → 0 responses.
- **45 s true promiscuous L2 listen** (no `-p`) for any MAC that is not the laptop or the HU → **0 frames**.
- Full MAC census across every capture of the day → exactly **two** real devices, ever:
  `30:13:8b:8e:a5:78` (laptop) and `9c:28:bf:0f:01:18` (head unit). Everything else in the census is
  a broadcast/multicast *destination*, not a device.
- HSFZ ident (`00 00 00 00 00 11`, UDP 6811), broadcast **and** unicast to `.90.33` / `.71.121`,
  repeatedly, across every wake state → **0 replies**.
- DoIP vehicle identification request on UDP 13400 → 0 replies.
- TCP 6801 / 6811 / 13400 to the known gateway IPs → timeout (contrast: the HU sends RST, i.e. it is
  *there*).
- IPv6: enabled on eth0, `ping ff02::1` all-nodes → only our own interface answered.
- **DHCP with an authoritative server already running at wake** → only ever `L7ENTRYHU`. The ZGW never
  sent a single Discover. This closes the `field-findings-2026-07-03.md:90` DHCP theory *by
  measurement*: a DHCP server cannot help a client that never transmits.

## 4a. The ZGW's identity, from the 2026-07-03 working capture

Comparing against `captures/klartext-session-2026-07-03.pcap` (when this car worked) pins the
gateway exactly:

| | Value |
|---|---|
| ZGW MAC | **`00:1a:37:26:54:29`** — OUI = **Lear Corporation** (BMW's gateway supplier) |
| ZGW IP | `169.254.90.33` |
| Spontaneous announcement | `169.254.90.33:6811 → 169.254.255.255:`**`7811`** |

With the MAC known, the strongest possible L2 probe was run: a **unicast ARP addressed directly to
`00:1a:37:26:54:29`** (`arp-scan --destaddr=…`), which bypasses all broadcast filtering. **No reply.**

| Capture | Frames with the ZGW MAC **as source** |
|---|---|
| 2026-07-03 (working) | **424** |
| Every capture on 2026-07-29 | **0** |

(A raw `ether host` count shows 8 today — all eight are *our own* unicast probes addressed *to* it.
Match on `ether src` when checking this, not `ether host`.)

## 4b. klartext gap found while doing this: the announcement port

`crates/hsfz/src/discover.rs` binds an **ephemeral** UDP port (`UdpSocket::bind((bind_ip, 0))`) and
only reads datagrams returned to that port. That covers *active* discovery — the gateway answering
our ident request — which is how 2026-07-03 succeeded.

It does **not** cover the gateway's *spontaneous* announcement, which the capture shows going to
**UDP 7811**. klartext listens on no such port, so a ZGW that announces itself at wake — potentially
before it will answer probes — is invisible.

`docs/superpowers/specs/2026-07-03-live-discovery-dynamic-core-design.md:302` records the
`6811→7811` direction, and `field-findings-2026-07-10-ios.md` recommends "sweep **or passive
listen**" — the passive-listen half was never implemented. Adding a 7811 listener to `discover()`
would make wake-time detection more robust. It would **not** have changed today's outcome (the ZGW
emitted nothing on any port), but it is a real gap confirmed against a real capture.

## 5. Wake sequences tried, all negative

- unplug → sleep 3–5 min → ignition ON → then plug in (the documented recovery)
- ignition ON → plug cable
- cable in → ignition off/on cycle
- engine started and running
- three battery resets, plus multiple ignition cycles

## 6. Topology notes (corrections to earlier assumptions)

- The **head unit is normally the only device on the wire until the ZGW joins** — documented in
  `field-findings-2026-07-03.md:93`. Its presence is expected and is *not* evidence of a fault.
- The HU is `L7ENTRYHU`, OUI `9C:28:BF` = **AUMOVIO Czech Republic** (Continental's automotive
  spinoff). It runs `Microsoft-WinCE/6.00`, serves HTTP 403 on :80, and opens
  `80, 443, 3500, 23000, 55555, 60222`. It exposes **no diagnostic endpoint** — it cannot substitute
  for the gateway.
- **The HU is the useful canary.** When it is reachable, the OBD Ethernet path is proven live, which
  is what makes a missing ZGW meaningful. When both are down, the two cases are indistinguishable.
- The ZGW's DHCP hostname, when it does appear, is `DIAGADR10BMWVIN<vin>` — worth grepping for.

## 7. ⚠️ Do not mass-scan car modules

A full `nmap -p 1-65535 -T4` against the head unit **crashed it** — the scan itself reported
`447 filtered (no-response)` as its stack gave out, the screen went to "no signal" then black, and it
recovered only via a watchdog reboot. Embedded automotive stacks do not tolerate this.

Use targeted probes. The `/24` ARP sweep found everything the `/16` sweep did, at a fraction of the
load. This crash did **not** cause the ZGW problem — the gateway was already absent on 2026-07-27,
before any scanning.

## 8. Conclusion and what is left

Every element outside the gateway has been eliminated with positive evidence. The fault is the F20's
ZGW: **its Ethernet interface does not come up, while the module itself functions on CAN.** This
matches the documented F-series failure mode ("ZGW not visible on ethernet" / gateways that work over
D-CAN but not Ethernet, i.e. corrupted Ethernet configuration).

Nothing further can be attempted from the Ethernet side — klartext is HSFZ-over-Ethernet only, and
there is no peer to talk to.

Remaining avenues, none of them klartext work:

1. **ICOM (A2/A3/Next)** — reported to succeed on ZGW work where plain ENET cables fail; it drives
   the OBD activation and session handshake itself rather than relying on the passive pin-8 resistor.
   This is the documented escalation.
2. **Recode / reflash the ZGW** to restore its Ethernet configuration, which needs (1).
3. A K+DCAN cable is **not** the answer — F-series is ENET (BN2020); K+DCAN is an E-series tool and
   would not give ISTA-grade access to an F20's gateway.

## 9. Reusable tooling from this session

`captures/zgw-watch.sh` (gitignored, root): waits for `eth0` carrier, captures **everything**
unfiltered to a timestamped pcapng plus a live log, survives unplug/replug by restarting on each
link-up, and prints a loud banner on any MAC that is not the laptop or the HU, on the
`DIAGADR10BMWVIN` DHCP hostname, and on any 6801/6811/13400 traffic.

```
doas bash captures/zgw-watch.sh
```

Captures contain the VIN. `captures/` is gitignored — never commit them, redact before sharing.
