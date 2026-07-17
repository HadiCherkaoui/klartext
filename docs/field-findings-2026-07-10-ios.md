# Field findings — iOS transport probe (2026-07-10)

Probe app on an iPhone with a USB-C Ethernet adapter, plugged into the ENET cable of the
**F25 X3** (the car-session-1 vehicle, gateway `169.254.71.121`). Raw output:
`captures/phone-probe-2026-07-10.txt` (gitignored — contains the VIN). These findings de-risk
**M11 Item 6 (mobile iOS)** at the transport level.

## Setup observed

- The adapter comes up as `en4` with an autoconfigured link-local address
  (`169.254.127.163/16`) — same `/16` as the gateway, no DHCP, no route needed.
- Cellular (`pdp_ip*`), VPN (`ipsec*`), and hotspot (`bridge100`) interfaces coexist;
  interface selection matters (see finding 3).

## Findings

### 1. Unicast HSFZ ident works — discovery needs NO multicast entitlement

A **unicast** UDP ident (`00 00 00 00 00 11` → port 6811, directly to the gateway IP) was
answered in **2 ms** with the standard 56-byte ident frame (control `0x0011`, payload
`DIAGADR10` + `BMWMAC…` + `BMWVIN…`). A probe to a neighboring empty IP (`….122`) timed out,
i.e. the reply is a real per-host answer, not broadcast leakage.

This kills the assumption that iOS discovery requires the
`com.apple.developer.networking.multicast` entitlement (broadcast ident). Two viable
entitlement-free strategies, both now evidence-backed:

- **Active sweep:** unicast-ident candidate IPs in parallel. Replies come in ~2 ms, so even a
  full `169.254.0.0/16` sweep with a few hundred concurrent sockets converges in seconds;
  a "last-known IP per VIN" cache makes the common case instant.
- **Passive listen:** the car-session-1 pcap shows the ZGW **self-announces** its ident 3× on
  the broadcast address on link-up/wake (t=581 s in the capture). Listening on UDP 6811
  requires no send entitlement at all.

### 2. Network.framework fails where BSD sockets succeed

`NWConnection` to `169.254.71.121:6801` (TCP, the HSFZ diag port) never left the waiting
state: `POSIXErrorCode(50): Network is down` — Network.framework refuses/misroutes the
IPv4 link-local destination in this multi-interface situation. A plain **POSIX `connect()`
with no bind succeeded in 1 ms** on the same phone, same cable, same moment.

Consequence for the Item 6 architecture: the plan (Rust core via UniFFI, which uses BSD
sockets through `std::net`/`tokio`) is **validated from real iPhone hardware** — and routing
the connection through Network.framework instead would have *broken* it. Keep the socket
path in Rust; don't wrap it in NWConnection.

### 3. Don't bind the UDP socket to the interface address

The ident retry **bound to en4's own address** (`169.254.127.163`) timed out, while the
unbound socket (default egress selection) got the 2 ms reply. On iOS, binding to the
interface *address* is not the way to pin the egress interface — if pinning is ever needed,
use `IP_BOUND_IF` (socket option) instead. For v1: don't bind; the connected/default path
works.

## Status for Item 6

| Question | Answer |
|---|---|
| Can an iPhone reach the gateway over USB-C Ethernet at all? | ✅ TCP 6801 connect in 1 ms |
| Discovery without the multicast entitlement? | ✅ unicast ident (+ passive announce as an option) |
| Does the Rust-core/BSD-socket plan survive contact with iOS? | ✅ — and NWConnection would not |
| Open | UniFFI packaging, background/session lifetime, App-Store-irrelevant (personal use) |
