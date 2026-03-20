# FlashSafe

A real-time desktop accessibility tool that detects and mitigates harmful flashing visual stimuli during gameplay or video playback.

> **Disclaimer:** FlashSafe is an accessibility aid, not a medical device. It does not certify content as medically safe.

---

## Crates

| Crate | Purpose |
|---|---|
| `flashsafe-core` | Detection and mitigation algorithms — no UI dependencies |
| `flashsafe-app` | Desktop application shell, capture pipeline, overlay |

---

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (stable, 1.75+)
- Windows 10/11 (DXGI Desktop Duplication requires Windows 8+)

---

## Building

```powershell
# Debug build
cargo build

# Release build (optimised)
cargo build --release
```

The release binary is written to `target/release/flashsafe.exe`.

---

## Running

```powershell
# Run with default log level (info)
cargo run --release

# Verbose logging
$env:RUST_LOG="debug"; cargo run --release
```

---

## Testing

```powershell
cargo test
```

The detection and mitigation modules have unit-test coverage. Run them before submitting changes.

---

## Architecture

See [`docs/adr/`](docs/adr/) for Architecture Decision Records.

- [ADR-001: Tech Stack](docs/adr/001-tech-stack.md)
