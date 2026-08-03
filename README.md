# banc

*banc d'essai* — a host-evaluated hardware-in-the-loop test framework for
embedded Rust. The harness is the bench; it is never the thing being tested
and never knows what's bolted to it.

A std `cargo test` suite on the host orchestrates the device under test
(probe-rs), assistant boards acting as off-target peripherals and observers
(postcard-rpc), and bench instruments. Assertions run host-side against
ground truth observed on the wire — never against the target's self-report
alone. On machines without a rig, suites report **ignored**, not passed.

Status: early scaffolding (0.0.x). APIs will churn.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and the
banc/essais responsibility boundary.

## Workspace

- `banc` — facade
- `banc-icd` — node-management + reference-assistant wire types (no_std)
- `banc-host` — fixtures, rig topology, runner (runtime self-skip), evidence
- `banc-probe` — target flash/reset/RTT via probe-rs
- `banc-instrument` — bench equipment, domain-neutral units
- `banc-assistant` — RP2350/embassy reference assistant firmware (own workspace)

## License

MIT or Apache-2.0, at your option. See NOTICE.md for prior-art credit.
