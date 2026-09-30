# ADR 001: Tech stack (FlashSafe)

## Status

Accepted

## Context

FlashSafe needs low-latency capture and GPU-friendly processing on Windows without injecting into games.

## Decision

- **Shell**: Tauri 2 + static web UI (Vite), always show control window; process exits when it closes.
- **Engine**: Rust on a dedicated thread using **Windows.Graphics.Capture** → D3D11 texture, CPU readback of a coarse grid for metrics, pixel shader mitigation, **mirror window** aligned to the target `HWND`.
- **Core logic**: `flashsafe-core` crate for config, downsample stats, detection heuristics, and mitigation parameters (unit-tested).

## Consequences

- WinRT + D3D11 interop is kept in the Tauri crate; core stays portable and testable.
- If WinRT from Rust becomes a maintenance burden, the same pipeline can move behind a WinUI shell with Rust as a DLL.
