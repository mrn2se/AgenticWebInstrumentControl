# AWIC — Development

How the code is organised and the rules it keeps. What AWIC does is in [FEATURES.md](FEATURES.md),
how it does it in [SPECIFICATION.md](SPECIFICATION.md). Building and running from source is in
[§7](#7-building-and-running-from-source), the container in [CONTAINER.md](CONTAINER.md).

## 1. Conventions

| | |
|---|---|
| Rust edition | 2021, workspace-wide `[workspace.dependencies]` |
| Errors | `thiserror` per crate, errors specific; `anyhow` only in `awic-server` main |
| Async | `tokio`; **no `block_on` outside tests**; no blocking syscalls in async fns (blocking work goes to `spawn_blocking`) |
| Logging | `tracing`; spans around device operations, `%device_id` as a field; human-readable on a TTY, JSON otherwise |
| Naming | crate `awic-x`, lib target `awic_x` |
| Tests | unit tests inline; integration tests in `tests/`; **no test may require hardware** (hardware tests are `#[ignore]`) |
| Formatting | `cargo fmt`; `cargo clippy --all-targets -- -D warnings` must pass |
| Frontend | no framework; web components; `strict: true`; no `any` |

**Hard rules** — the design depends on them:

1. `awic-core` names **no driver-specific type**: no `nusb` type in its public API, no driver
   error in its error enums. A new backend must be addable without touching core.
2. Device identity is the **bus path**, never the device number, never VID:PID alone.
3. Soft limits and USB port protection are enforced **in core**, not per transport.
4. Measurements run on **full-resolution** data, server-side.
5. Sample data never travels as JSON.

## 2. Pure crates

`awic-record`, `awic-pattern`, `awic-decode` and `awic-scpi` depend on **no tokio and do no I/O**;
`awic-proto`'s decode types need nothing beyond `std` and `serde`. The server owns every task and
socket. Each pure crate ships a throughput test or benchmark where the specification names a rate.

Decoders and pattern matchers count in **sample indices**, not nanoseconds (SPEC §15, D-10): exact
at every rate, and the caller knows the rate.

Shared files (`awic-proto/src/control.rs`, `awic-server/src/dispatch.rs`, the workspace members)
take small, additive edits.

## 3. Crates and repository layout

```
awic-proto    wire types; exports TypeScript via ts-rs   (no awic deps)
awic-core     traits, registry, capture store, event bus, config, soft limits, measurements
awic-usb      topology tree, hub port power, device watcher (nusb)
awic-fake     synthetic Backend for tests and the UI without hardware
awic-native   FX2 firmware upload, fx2lafw and fx2scope drivers
awic-record   live recorder: runs, compressed blocks, summary tree       (pure)
awic-pattern  pattern language, search and incremental matching          (pure)
awic-decode   UART, I²C, SPI, CAN, 1-Wire, channel survey                (pure)
awic-scpi     SCPI parser and command tree                               (pure)
awic-render   server-side views: waveform, eye, histogram, anomalies
awic-server   the `awic` binary: axum, WS, MCP (src/mcp/), SCPI TCP, recorder, terminal
```

```
.
├── Cargo.toml, crates/           the Rust workspace above
├── web/                          frontend: src/{modules,render,proto,shell,store,ws}, e2e/
├── config/awic.toml              default configuration; config/firmware/ for firmware overrides
├── Containerfile                 the container image (CONTAINER.md)
├── testdata/bench/               captures recorded on the bench, used by tests
├── example-report/               a bench surveyed over MCP: views and MCP tab screenshots
├── tools/
│   ├── run.sh, run-rootful.sh    run the container image (make run, make run-rootful)
│   ├── udev/                     udev rules and their installer (make udev)
│   ├── visa/                     pyvisa conformance check of the SCPI port (make visa)
│   └── licenses/                 THIRD-PARTY-LICENSES.txt generator (make licenses)
└── docs/
```

`web/src/proto/types.ts` is generated from the Rust types by ts-rs and not in source control:
`make types` (or any `cargo test -p awic-proto export_bindings`) writes it, and every frontend
build needs it first.

## 4. Tests

```sh
make test                         # cargo test, clippy -D warnings, fmt --check
cd web && npm test                # vitest
cd web && npm run typecheck
make e2e                          # Playwright UI smoke test against a running server (container)
make visa                         # pyvisa checks against a running server's SCPI port (container)
```

- `awic-fake` makes protocol, UI, MCP and SCPI testable with no hardware; `make e2e` runs
  `web/e2e/ui-smoke.js` and works against a server started with only fake instruments.
- Hardware tests are `#[ignore]` and run on the bench:
  `cargo test -p awic-native -- --ignored`.
- Two Rust tests need a readable `/sys/bus/usb` (`a_lock_is_persisted_and_survives_a_restart`,
  `reenumeration_wait_times_out_when_nothing_matches`) and fail in a sandbox without one.
- Decoder tests run on synthesised waveforms (with chunk boundaries at every position) and on
  real captures in `testdata/bench/`.
- `sigrok-cli` is the oracle for driver work: capture the same signal with both and compare.
- Bug and feature numbers (B-nn, F-nn) are shared with GitHub issues: take the next number after
  the highest one used.

## 5. Risks the design guards against

| # | Risk | Guard |
|---|---|---|
| R3 | High-rate bulk streaming drops samples | several `nusb` transfers in flight; sequence numbers on the wire detect drops |
| R5 | A power cycle loses the FX2 firmware | `prepare()` uploads firmware on every enumeration |
| R7 | An agent or SCPI client pulls multi-MB captures | handles plus server-side decimation; no tool returns bulk samples |
| R9 | Two devices share `0925:3881` with no serial number | identity by bus path; the UI labels by place |
| R11 | The recorder falls behind the device | digital append is `O(edges)`; ingest off the async executor (`spawn_blocking`); ≥ 10× real time at 24 MS/s in a test |
| R12 | Analog recording at 16 MS/s exhausts memory | a hard memory budget, eviction from the left, retention shown live |
| R13 | A tty `open()` blocks waiting for carrier | always `O_NONBLOCK \| O_NOCTTY` |

## 6. Releases

Create a release in GitHub (Releases → Draft a new release) with a new tag `v<semver>`, e.g.
`v0.2.0` or `v0.3.0-rc.1`, and publish it. [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)
then builds from that tag and attaches the files below. It builds only when the tests pass and
the dependency audit is clean: no known advisory against any Rust crate (`cargo audit`, also
failing on unmaintained or yanked crates) or npm package (`npm audit`, build tools included),
and every npm package's registry signature verified.

- `awic-linux-x86_64`, `awic-linux-aarch64`, `awic-linux-armv7`: statically linked (musl)
  executables with the web UI compiled in (`--features embed-web`). aarch64 is built natively on
  an ARM runner; armv7 is cross-built with the toolchain's `rust-lld` (`.cargo/config.toml`) and
  its tests run under qemu.
- `awic-linux-<arch>.cdx.json`, `awic-web.cdx.json`: CycloneDX SBOMs of each executable's Rust
  crates and of the web UI's runtime npm packages.
- `THIRD-PARTY-LICENSES.txt`: the license notices of the third-party code compiled into the
  executables, one file for every target (Rust crates, the Rust standard library and musl, the
  web UI's npm packages), which their licenses require to go with them. [`tools/licenses/generate.sh`](../tools/licenses/generate.sh)
  writes it; `make licenses` builds it locally (needs `cargo install --locked cargo-about --features cli`).

The tag is the version: the workflow writes it into `crates/awic-server/Cargo.toml` and
`Cargo.lock` for the build, so no version bump commit is needed first.

## 7. Building and running from source

**Requirements:**

- Linux (AWIC uses usbfs, sysfs and netlink directly).
- Rust: [`rust-toolchain.toml`](../rust-toolchain.toml) pins the version; `rustup` picks it up.
- Node.js 22 and npm, for the web UI. Without Node on the host, `make web-install` and
  `make web-build` run npm in a container.
- The firmware package and the udev rules, as for the release binary ([README](../README.md#install));
  `make udev` installs the rules from the checkout.

```sh
make run-backend      # build server and web UI, serve on http://127.0.0.1:8080/
make run-lan          # the same on all interfaces: HTTP 0.0.0.0:8080, SCPI 0.0.0.0:5025
```

`run-backend` uses [`config/awic.toml`](../config/awic.toml) with the web UI from `web/dist` and the
session state in `./state`. Server arguments and settings pass through:
`make run-backend ARGS="--set recorder.memory_mb=512"`, `AWIC_AUTH__TOKEN=secret make run-lan`.

| Target | Does |
|---|---|
| `make types` | generates `web/src/proto/types.ts` from the Rust wire types (ts-rs); the web UI needs it first |
| `make backend` | `cargo build --release --locked -p awic-server --bin awic` → `target/release/awic` |
| `make frontend` | `npm ci` when needed, then the web UI into `web/dist` |
| `make native` | one self-contained `awic` with the web UI compiled in (`--features embed-web`), like the release binary |
| `make run-frontend` | Vite dev server with hot reload on http://127.0.0.1:5173/, proxying to a running `run-backend` |
| `make test` | `cargo test`, `clippy -D warnings`, `fmt --check` |

Run the binary directly:

```sh
./target/release/awic --config config/awic.toml \
  --set server.static_dir="$PWD/web/dist" --set server.state_dir="$PWD/state"
```

A binary built with `make native` needs no `static_dir`.

**For a Raspberry Pi or another ARM board,** build on any Linux x86_64 host; no cross compiler is
needed, since `.cargo/config.toml` links the ARM targets with the toolchain's own `rust-lld`.
Build the web UI first (`make frontend`), then:

```sh
rustup target add aarch64-unknown-linux-musl          # or armv7-unknown-linux-musleabihf
cargo build --release --locked --target aarch64-unknown-linux-musl \
  -p awic-server --bin awic --features embed-web
scp target/aarch64-unknown-linux-musl/release/awic pi@raspberrypi:
```
