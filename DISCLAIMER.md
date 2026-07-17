# Disclaimer & legal notice

**klartext is an independent, personal-use interoperability and educational project.**
It is not affiliated with, authorized by, endorsed by, or connected to BMW AG or any of
its subsidiaries. "BMW", "ISTA", "Rheingold", "EDIABAS", and related marks are the
property of their respective owners and are used here only nominatively, to describe what
the software interoperates with.

## Bring-your-own-data — no proprietary data is distributed

This repository ships **no BMW data of any kind** — no ISTA/Rheingold databases, no
PSdzData, no SGBD (`.prg`) files, no packet captures, and no vehicle identifiers (VINs).
Its full history has been audited to confirm this.

The semantic layer works only against data **you supply yourself** from an ISTA
installation **you are licensed to use**. The build script
(`scripts/build-semantic-db.sh`) reads *your own* local files and writes a gitignored
artifact; it embeds nothing and uploads nothing. The RC4 password it references is the
public .NET strong-name token of a Microsoft-signed ISTA assembly (a published value, not
a secret), and it is useless without the encrypted databases that only your own ISTA
install provides. You are responsible for ensuring your use of any BMW software and data
complies with the licenses and terms you accepted for it, and with the laws of your
jurisdiction.

## How the protocol layer was built

The transport (HSFZ) and diagnostic (UDS / ISO 14229) layers are reimplemented from
public protocol write-ups, ISO standards, and observation of the tester's own traffic.
Frame layouts and handshakes are facts, not copyrightable expression. **No code is copied
from any reference library** — in particular none from GPL-licensed projects such as
Scapy or ediabaslib; those were read only to understand behavior and reimplemented in
original code (see `docs/protocol-reference.md`).

## Talking to a car changes a car

This software communicates directly with vehicle control units. Even the limited writes it
exposes — clearing fault memory, and the gated low-risk service functions in the CLI —
**change vehicle state**: they discard freeze-frame data, can reset readiness monitors, and
interact with safety-relevant systems. Reads are safe; writes are your responsibility.

**Use entirely at your own risk.** As stated in the GNU AGPL v3 (sections 15–16), the
software is provided **without any warranty**; the authors are not liable for any damage
to your vehicle, data, or anything else arising from its use. Do not use it on a vehicle
you do not own or lack permission to service, do not use it while driving, and do not rely
on it for anything safety-critical.

## License

klartext is licensed under the **GNU Affero General Public License v3.0** (`LICENSE`).
