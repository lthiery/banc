# banc architecture

banc (from *banc d'essai*, French for test bench) is a host-evaluated
hardware-in-the-loop test framework for embedded Rust. The harness is the
bench: it is never the thing being tested and never knows what's bolted to it.

## The model

A std `cargo test` suite on the host (tokio) orchestrates three kinds of
hardware:

- the **target** — the device under test, flashed/reset/observed via probe-rs;
- **assistants** — boards acting as off-target peripherals and observers
  (GPIO peer, UART peer, SPI/I2C controller, timestamped edge capture),
  speaking `banc-icd` over postcard-rpc USB;
- **instruments** — bench equipment (attenuators, analyzers) addressed in
  their own physical units.

Test evaluation happens **on the host**, against ground truth observed on the
wire by assistants and instruments — never against the target's self-report
alone. RTT/defmt output from the target is *evidence* attached to failures,
not the assertion surface.

```
┌──────────────────── host (cargo test, std, tokio) ────────────────────┐
│ essais (downstream) ── banc fixtures: Target, Assistant, Instrument   │
│    │ banc-probe: probe-rs (flash, reset, RTT/defmt evidence)          │
│    │ banc-host: postcard-rpc HostClient (USB/serial) ─────┐           │
└────┼──────────────────────────────────────────────────────┼───────────┘
     ▼                                                      ▼
 [ test target ] ◄── real wiring (SPI/I2C/UART/GPIO/RF) ► [ assistant(s) ]
                                                            ▲
                 [ instruments: RCDAT, SCPI/VISA, Saleae Logic 2 gRPC ]─┘
```

## The responsibility boundary

For every module, type, and endpoint: **"Would this exist if the DUT were a
CAN transceiver?"** If no, it does not belong in banc. `grep -ri` for any
protocol domain vocabulary over banc source must return nothing.

Downstream test suites are called **essais** by convention (e.g.
`lora-rs/essais`). An essai provides:

1. its **ICD crate(s)** — postcard-rpc endpoint/topic definitions for its own
   nodes (banc carries only the node-management + reference-assistant ICD);
2. **domain fixtures** wrapping banc's (e.g. a "network server" fixture built
   on banc's runner and evidence);
3. the **test suites** themselves, plus domain firmware for targets/nodes.

Two hard rules learned from prior art (embedded-test-stand, whose author
flagged his shared firmware-lib as a misdesign):

- **No HAL code in shared infrastructure.** Anything with a HAL dependency
  belongs in a specific firmware crate, never in banc's generic layer.
  `banc-assistant` is a *reference implementation* for one board (RP2350),
  not a firmware library.
- **Timing assertions use assistant-local timestamps.** Host wall-clock over
  USB/serial is for narrative ordering in evidence logs only.

## Crates

| Crate | Layer | Contents |
|---|---|---|
| `banc` | facade | re-exports + prelude |
| `banc-icd` | wire (no_std) | node management (identify/reset) + reference-assistant v0 endpoints (GPIO, pin monitor, UART, SPI/I2C, timestamped capture) |
| `banc-host` | host | rig topology config (`banc-rig.toml`), cross-process rig lock, `Node` discovery/handshake, libtest-mimic runner with runtime self-skip, per-test evidence, expect helpers |
| `banc-probe` | host | target lifecycle via probe-rs: local (library, dedicated session thread) and remote (`probe-rs serve` via CLI shell-out; see below) |
| `banc-instrument` | host | `Instrument` trait + setpoint parameterization; domain-neutral drivers |
| `banc-assistant` | firmware | RP2350/embassy reference assistant implementing `banc-icd` (own workspace, excluded from the host build) |

## Consumption model (postcard-rpc)

postcard-rpc keys are structural — FNV1a-64 over (path, schema) — so ICDs
compose without central registration. banc-host stays fully generic: its
`HostClient` wrapper only names banc's own node-management endpoints;
consumer endpoints are per-call generics on the client the consumer gets via
`Node::client()`.

The one obligation that cannot be removed: a consumer building **combined
firmware** (their endpoints + banc's on one node) must enumerate banc's
endpoints in their single `endpoints!`/`define_dispatch!` table. `banc-icd`
exports the wire types and canonical path constants so those rows hash to
identical keys. This is a documentation contract, not a code dependency.

`HostClient` has no auto-reconnect (a closed client and all clones are dead);
banc-host owns re-enumeration and reconnection.

## Known seams / trade-offs

- **probe-rs remote is CLI-only.** The `remote` feature (websocket RPC to
  `probe-rs serve`) lives in the `probe-rs-tools` binary and is not exported
  as a library. `banc-probe` therefore does the honest thing locally (library
  API: flash, reset, RTT into evidence) and shells out to a remote-capable
  `probe-rs` binary for remote rigs. If upstream ever exports the RPC client,
  `banc-probe/src/remote.rs` is the seam to replace. Remote RTT capture is
  not wired yet.
- **Self-skip is runtime, not compile-time.** libtest-mimic 0.8.2's
  `Trial::ignorable_test` + `Completion::ignored_with(reason)` reports
  rig-less machines as *ignored* — never silently passed. A configured rig
  that fails to initialize is a **failure**, not a skip.
- **Exclusivity is process-wide and cross-process.** In-process, trials run
  on one thread. Cross-process (nextest runs one process per test; CI pollers
  and humans coexist), an advisory file lock is held for the life of the
  `Rig` fixture.
- **One tokio runtime per trial.** The runtime is dropped when the trial
  ends, so every task a test (or its fixture libraries) spawned is torn down
  before the next trial — leaked tasks cannot hold sockets or hardware
  across tests. Learned live: a shared runtime let a fixture library's
  internal tasks keep a UDP port bound into the next scenario. Consequence:
  the shared `Rig` holds only runtime-independent resources (config, file
  lock); connections belong to per-test fixtures.

## Prior art

Architecture informed by
[hannobraun/embedded-test-stand](https://github.com/hannobraun/embedded-test-stand)
(0BSD, archived 2023) — see NOTICE.md. Its target/assistant/node/suite
vocabulary and lock-owned-by-fixture pattern survive here; its shared
firmware library deliberately does not (the author's own conclusion, issue
#85).
