# FlashSafe

A Windows accessibility tool for light-sensitive players. FlashSafe captures a game window and shows a filtered copy on top of it. Normal scenes pass through untouched, but when a large area suddenly brightens, the brightening is ramped in gradually. Strobes and flicker are flattened to a steady, dim image.

It's meant for single-player and casual co-op games. It uses compositor capture (the same mechanism as Game Bar recording) and never injects into the game. Anti-cheat systems may still object to screen capture.

> **Not a medical device.** FlashSafe reduces flashing but cannot guarantee every flash is caught. If you feel unwell, stop playing.

**Status:** pre-MVP. The flash filter is done and tested, and the filtered picture is drawn in a click-through overlay that never takes focus, so mouse, keyboard and controllers keep going to the game. See [docs/mvp-plan.md](docs/mvp-plan.md) for the audit, the design and the roadmap.

## How the filter works

FlashSafe remembers what was on screen last frame, pixel by pixel, in linear light (the units WCAG's flash rules use).
- A pixel may get brighter or darker only at a limited rate. That rate is budgeted per screen area about the size of your central field of view.
- Small or moving details (a bright object, a panning texture) pass through untouched. A flash or cut that brightens or darkens a real part of the screen is turned into a short fade.
- Pixels that aren't changing, such as a HUD or a menu bar, are never touched. Flashing pixels right next to them are still limited.
- When back-and-forth flicker is detected, that area switches to much slower rates for a few seconds, so a strobe becomes a steady, dim image.

The test suite scores the filtered output with a WCAG "three flashes per second" judge. It covers every preset, 30/60/144 fps and jittered frame timing.

## Use it

1. Set the game to **borderless windowed** (or windowed). Exclusive fullscreen can't be captured.
2. Pick the game window in FlashSafe, choose a strength (Low / Medium / High), and press **Start protection**.

Settings: `%APPDATA%\FlashSafe\settings.json`. Logs: `%APPDATA%\FlashSafe\logs\` (please attach them to bug reports).

## Develop

Prerequisites (Windows 10/11): [Rust](https://rustup.rs/) (stable), [Node.js](https://nodejs.org/) 18+, and Visual Studio Build Tools with the C++ workload and a Windows SDK.

```powershell
npm install
npm run tauri dev        # run the app with hot-reloading UI
npm run tauri build -- --no-bundle   # release build → target/release/flashsafe.exe
cargo test -p flashsafe-core         # filter + WCAG test suite (any OS)
```

A test window for checking FlashSafe without a game:

```powershell
cargo run -p flashsafe --bin flashsafe-harness
```

It flashes on demand (keys `0` slow toggle, `1`–`5` strobe at 3–20 Hz, `D` dark, `W` white) and shows every input it receives: clicks, wheel, keys, raw mouse deltas and XInput controller state. With protection on, all of them should keep counting, and its red crosshair should sit exactly under the real cursor.

### Working from Linux or macOS
The filter crate builds and tests anywhere. The Windows app can be type-checked without a Windows machine:

```sh
rustup target add x86_64-pc-windows-msvc
cargo clippy --target x86_64-pc-windows-msvc --workspace --all-targets -- -D warnings
```

### Layout

| Path | What |
|------|------|
| `crates/flashsafe-core` | Portable, tested core: colour math, tile stats, WCAG judge, the filter, config/presets |
| `src-tauri/src/win/engine.rs` | Windows capture (WGC) → filter → mirror window |
| `src-tauri/src/shader.hlsl` | Applies the per-tile gain in linear light |
| `src/` | Settings UI (Vite, plain JS) |
| `docs/` | [MVP plan](docs/mvp-plan.md), [display mode notes](docs/game-mode-matrix.md), [ADR](docs/adr/001-tech-stack.md) |
