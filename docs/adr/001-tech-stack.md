# ADR-001: Tech Stack

**Date:** 2026-03-19
**Status:** Accepted
**Deciders:** Founding Engineer

---

## Context

FlashSafe must capture the entire desktop or a fullscreen window, analyse each frame for harmful flashing content, and optionally overlay a mitigation layer — all within a 16 ms budget at 60 fps on a commodity gaming PC (typically a mid-range Windows 10/11 machine with a dedicated GPU).

We need to decide on:

1. **Implementation language**
2. **Screen capture API**
3. **GPU acceleration strategy**
4. **Application shell / tray integration**
5. **Configuration format**

---

## Decision

### 1. Language: Rust (stable)

**Chosen over:** C++, Go, Python

- Zero-cost abstractions give us the throughput needed for per-frame pixel analysis.
- Memory safety eliminates an entire class of latency-causing bugs (use-after-free, data races).
- Excellent Windows FFI support (`windows-rs`) for DXGI and D3D11.
- Single cross-compiled binary; no runtime dependency distribution.
- Strong ecosystem: `tracing` for structured logging, `anyhow`/`thiserror` for ergonomic error handling.

### 2. Screen Capture: DXGI Desktop Duplication API

**Chosen over:** BitBlt/GDI, OBS virtual camera, Mirror Driver

- Hardware-accelerated; frames surface as GPU textures (ID3D11Texture2D), avoiding a CPU roundtrip.
- Latency typically < 1 ms from vblank — far below our 16 ms budget.
- Works across DX11/DX12 games and the Windows desktop compositor.
- No kernel driver required; user-mode only.
- Requires Windows 8+; acceptable given our target audience (Windows 10/11 gaming PCs).

### 3. GPU Acceleration: WGPU (optional, CPU fallback required)

**Chosen over:** raw D3D11 compute, OpenCL, CUDA

- Cross-backend (D3D12, Vulkan, Metal, D3D11) via a single API — keeps the door open for future macOS/Linux support.
- CPU fallback path means FlashSafe works on integrated graphics and virtual machines.
- v0.1 ships a CPU-only path; GPU shaders are a v0.2 optimisation once correctness is established.

### 4. Application Shell: `egui` + `tray-item` crate

**Chosen over:** Tauri, Qt, Win32 dialogs

- `egui` is immediate-mode, runs on top of `winit`/`wgpu`, and adds < 3 MB to the binary.
- `tray-item` provides a system-tray icon with a minimal Rust API.
- No Electron/Node dependency; no web view security surface.
- Tauri was considered but adds a Chromium/WebKit dependency and complicates the build significantly for v0.1.

### 5. Configuration: TOML

**Chosen over:** JSON, YAML, INI

- Human-readable and writable; easy for users to hand-edit sensitivity profiles.
- `toml` crate has strong serde integration.

---

## Workspace Layout

```
FlashSafe/
├── Cargo.toml              # workspace manifest
├── crates/
│   ├── flashsafe-core/     # detection + mitigation (no UI deps)
│   └── flashsafe-app/      # desktop shell, capture, overlay
├── docs/
│   └── adr/
│       └── 001-tech-stack.md
├── .gitignore
└── README.md
```

`flashsafe-core` deliberately has no dependency on `flashsafe-app` or any UI crate. All detection and mitigation logic can be unit-tested in isolation.

---

## Consequences

- **Positive:** Single binary deployment, low latency capture path, testable core logic.
- **Positive:** GPU path deferred to v0.2 without blocking correctness work.
- **Negative:** Rust compile times are longer than C/Go for first builds (mitigated by `sccache` in CI).
- **Negative:** DXGI Desktop Duplication is Windows-only; macOS/Linux support requires separate capture backends in a future phase.
