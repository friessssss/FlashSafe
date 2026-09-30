# FlashSafe → MVP: audit + implementation plan

## Status
- **Phase 0 — done:** CI restored, real icons, generated schemas ignored, file logging, shell plugin dropped, CSP set, README rewritten.
- **Phase 1 — done** except the `flashsafe-sim` PNG-sequence tool: WCAG judge, per-pixel filter with area budgets, presets and config migration. 33 tests.
- **Engine:**
  - The GPU does the per-pixel work: a stats pass, then a mip-chain area average, then an apply pass that keeps a display history.
  - The CPU runs the tile and region budget on a 160×90 readback.
  - Only the target's **client area** is captured and covered (crop via `DWMWA_EXTENDED_FRAME_BOUNDS`), and the frame pool is recreated when the window is resized.
- **Phase 2 — implemented, awaiting Windows validation** (`src-tauri/src/win/overlay.rs`):
  - Click-through overlay: `WS_EX_NOREDIRECTIONBITMAP | LAYERED | TRANSPARENT | TOPMOST | NOACTIVATE | TOOLWINDOW`, presented through a DirectComposition swapchain (`FLIP_SEQUENTIAL`, frame-latency waitable, max latency 1).
  - Never activates. Topmost while the game (or a window it owns) is in front; otherwise sits directly above the game in z-order. Hidden while the game is minimized. Excluded from screen capture.
  - Placement is polled every engine tick (cheap Win32 calls; `SetWindowPos` only on change) instead of WinEvent hooks.
  - The session stops, with a message, if the game window closes or the engine hits an error, rather than leaving a frozen picture over the game.
  - The harness is now an input test target (clicks, wheel, keys, raw mouse, XInput, crosshair, strobe keys).
  - Not done: exclusive-fullscreen and HDR detection and warnings (Phase 4).
- **Phases 3–6:** not started.

## Context

FlashSafe is a Windows accessibility tool for light-sensitive players. It captures a game window and shows a flash-mitigated copy on top of it. The current build does three things badly:
- It breaks mouse, keyboard and controller input.
- It adds latency.
- It barely changes the picture.

Decisions from the user:
- **Keep the mirror approach.** Hide the game behind a processed copy, so flashes never reach the eyes unfiltered. Rebuild it properly.
- **Distribution:** unsigned installer published as a GitHub Release.
- **MVP display target:** a single SDR monitor in borderless or windowed mode. Multi-monitor, HDR and exclusive fullscreen are later work. The app will detect those setups and warn the user instead of failing silently.

---

## Part 1 — Audit (what's wrong today)

### Why input breaks (root causes)
1. **`HTTRANSPARENT` click-through only works within one thread.** `WM_NCHITTEST → HTTRANSPARENT` (`src-tauri/src/win/engine.rs:333`) passes the hit test only to windows owned by the *same thread*. The game belongs to another process, so clicks land on the mirror and are swallowed. The fix is `WS_EX_LAYERED | WS_EX_TRANSPARENT`. In the current code that combination can't be used, because it doesn't work with the HWND flip swapchain.
2. **The mirror competes for focus and z-order.**
   - `SetWindowPos(HWND_TOPMOST, SWP_SHOWWINDOW)` runs on every move.
   - The window has no `WS_EX_TOOLWINDOW` flag.
   - Nothing tracks which window is in the foreground, so the mirror stays topmost over *everything* even after alt-tab.
   - XInput and raw-input games only read input while they are the foreground window. Any focus theft kills the controller.
3. **The cursor is captured.** WGC captures the cursor by default, so it's baked into a frame that's ~1–2 frames old: you get a double or laggy cursor. Win11 also draws a yellow capture border.
4. **Alignment is wrong.** `GetWindowRect` includes the invisible DWM resize borders, and the capture includes the title bar and frame. The mirror is sized once from the first frame and the swapchain is never resized. Nothing handles DPI either. Result: the image is offset or stretched, which is confusing to aim with even when clicks do pass through.

### Why it "didn't work great" (pipeline and algorithm bugs)
- **Temporal blend is a no-op.** `srv0` and `srv1` are both views of the current frame (`engine.rs:787-789`), and no previous-frame texture exists. The "smoothing" never ran.
- **The highlight knee brightens.** `reinhard_knee` in `shader.hlsl` with k<1 *boosts* highlights (lum 0.9 → ~1.25 at k=0.74). All the math also runs in gamma space, not linear.
- **Detection is weak.**
  - It point-samples one pixel per grid cell from a 320×180 bilinear downscale with no mips, which aliases badly.
  - It uses a single global mean.
  - It fires on dark→bright *and* bright→dark changes, and on scene cuts.
  - The band-pass filter assumes a fixed 120 Hz, but WGC frames arrive irregularly.
  - Nothing follows WCAG (area, opposing-transition pairs, 3/sec, red flash). A WCAG detector and a red-flash detector existed and were deleted in `1651294`; see `git show HEAD~1:crates/flashsafe-core/src/detection.rs`.
- **The mitigation model is wrong for the goal.** It computes a threat score, then applies whole-screen exposure scaling. It doesn't limit the *rate of luminance change*, which is what makes a flash a flash. A strobe can make the dimming itself pump, so FlashSafe becomes a flicker source.
- **Latency and stutter:**
  - A blocking `Map()` on the staging texture stalls the CPU waiting for the GPU on every frame.
  - `sleep(present_delay_ms)` on the hot path.
  - The frame pool has 2 buffers but the channel queues 8 frames (it holds frames and doesn't always drop to the newest).
  - `Present(0)` has no latency control.
  - SRVs and RTVs are re-created every frame.
  - The engine loop busy-polls every 2 ms.
- **Robustness:**
  - Nothing handles `GraphicsCaptureItem.Closed`, device loss, a minimized target or target resize.
  - The `WM_DESTROY` filter in `pump_messages` is odd.
  - `.unwrap()` calls sit in the hot path.
  - The Stop path leaves the mirror class registered, which is harmless but sloppy.

### Project hygiene
- The CI workflow was deleted in `1651294`, so nothing builds or lints Windows code.
- `src-tauri/icons/icon.ico` is **0 bytes**, which breaks bundling and likely `tauri-build`. `gen/schemas/*.json` has 0-byte files too.
- Bundling is disabled. Logging (`tracing` plus rotation) was removed. `tauri-plugin-shell` is unused. CSP is `null`.
- The UI exposes 11 expert sliders plus "present delay". There's no tray icon, no hotkey and no safety disclaimer.
- The README has Cursor-specific troubleshooting and describes behavior that doesn't exist.

**What's worth keeping:**
- The Tauri shell and command wiring (`src-tauri/src/lib.rs`).
- The config load/save and clamp pattern (`crates/flashsafe-core/src/config.rs`).
- The WGC and D3D device bootstrap (`create_capture_item`, `create_d3d11_device`, `create_winrt_device`, `compile_shaders` in `engine.rs`).
- The window enumeration code.
- The harness binary.
- The WCAG detectors in git history.

---

## Part 2 — Target architecture ("mirror done right")

Magpie, the open-source game-window upscaler, uses essentially the same design. It's the reference to follow for the overlay window, cursor and focus handling.

```
Game HWND ──WGC (cursor off, border off, free-threaded pool, newest-frame-only)──►
  GPU: (1) linearize + tile-luminance downsample (compute)
       (2) per-tile state update: slew-limited displayed luminance + strobe activity (compute, state textures ping-pong)
       (3) full-res apply: frame × bilinear(tile gain), crop to client area (pixel shader)
  ──► DirectComposition swapchain on click-through overlay HWND ──► DWM
  (async, non-blocking readback of a tiny stats texture → UI/tray, never on the critical path)
```

### Overlay window (fixes input)
- Window styles: `WS_POPUP` with `WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`.
- Present through `CreateSwapChainForComposition` plus an `IDCompositionDevice` target/visual. This is what makes a layered, input-transparent window compatible with flip-model presentation.
- The overlay never activates and never calls `SetForegroundWindow`, so the game keeps focus for keyboard, raw input and XInput.
- Call `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` so the overlay never feeds back into capture, OBS or Discord.
- Use `SetWinEventHook` for `EVENT_SYSTEM_FOREGROUND`, `EVENT_OBJECT_LOCATIONCHANGE` and `EVENT_SYSTEM_MINIMIZESTART/END` instead of per-frame polling:
  - Show the overlay only while the game or its child windows are in the foreground.
  - Hide it on alt-tab or minimize.
  - Reposition it on move or resize.
- Geometry:
  - Get the capture origin from `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`.
  - Get the client rect from `GetClientRect` + `ClientToScreen`.
  - Crop the source rect in the shader so the overlay covers exactly the client area.
  - Confirm the process runs as PerMonitorV2 DPI aware.
  - When the frame `ContentSize` changes, call `FramePool.Recreate` and `ResizeBuffers`.
- Capture session settings: `IsCursorCaptureEnabled(false)` so the real hardware cursor stays on top, and `IsBorderRequired(false)` on Win11, requesting `GraphicsCaptureAccess` when needed.

### Filter algorithm: per-pixel rise/fall limiter with area budgets and strobe hold
All math runs in linear light using WCAG relative luminance.
- **Per pixel, state = the colour displayed last frame** (`C`, luminance `P`). For input `x` with luminance `L`:
  - **Brightening** (`L > P`): show `x` dimmed to `P + s_rise·(L − P)`.
  - **Darkening:** crossfade `lerp(C, x, s_fall)`. A gain can't hold a pixel brighter than a frame that went black, so a crossfade is needed here.
  - Pixels that aren't changing are never touched.
- **Per tile (32×18), the *net* change `L_tile − P_tile` is budgeted.** A detailed texture panning across the screen changes many pixels but nets out to about zero, so it passes. A flash is a coherent net change.
  - Only one side is limited per tile, so the displayed tile mean lands exactly on the allowed value.
  - Per-pixel detail only decides *which* pixels absorb the limit.
- **Per region** (a full-size window as big as WCAG's 10° field, 11×7 tiles, shifted inward at screen edges): two token buckets bound the displayed average. It may rise at most `rise·(T + burst_secs)` and fall at most `fall·(T + burst_secs)`. Each tile takes the strictest scale of any covering window, so the bound holds for every region-sized window.
- **Strobe hold:** hysteresis-counted opposing transitions in a region's input switch it to slow `hold_rise`/`hold_fall` rates until `hold_secs` after the strobing stops. The triggers are set so that a single flash doesn't trigger hold.
- **Interpolation:** per-tile scales are interpolated bilinearly per pixel. First, rise scales are eroded (3×3 min) and fall scales dilated (3×3 max, ignoring tiles with no darkening). That way no pixel is limited less strictly than its own tile requires.
- **Gain floor:** `min_gain ≤ 0.06` on brightening pixels. A floor near 0.1 would let a WCAG-sized jump through on its own.
- **Why per-pixel (history).** The first per-tile-gain version left a bright bar under the harness title bar. The title bar and the flashing pixels shared a tile, so the flash there was under-protected by about 0.22 luminance, and interpolation spread it. Bright HUD elements next to flashes in real games hit the same problem.
- **Why falls are limited too.** With instant darkening, a single black frame in a bright scene became a drop followed by a ~1 s ramp back up, which is worse than the blip itself. It's now a shallow dip that recovers in about 8 frames, and cuts to black become short fades.
- **Tests** (`crates/flashsafe-core/src/filter.rs`, through `PixelFilter`):
  - Strobe cases, each ≤ 3 flashes/s by the WCAG judge in `wcag.rs`:
    - full-screen, localized, dark-on-bright and red strobes;
    - every preset, at 30/60/144 fps, with jittered frame times.
  - Title-bar band and HUD bar next to strobes: untouched, with no bright bar or halo.
  - Textured pan: passes through with under 0.02 mean error.
  - Moving bright and dark objects: not dimmed or smeared.
  - Other cases: single dark frame, cut to black, camera flash, scene cut, hold engage/release, long frame gaps.
- **Not yet:** a dedicated red-flash term (saturated red ↔ another hue at equal luminance). Red strobes against dark are already caught through luminance.
- **The engine must keep re-presenting** the last frame while `TileFilter::is_settled()` is false, because WGC only delivers frames when content changes.

### Latency budget
The target is about 1 frame plus DWM composition over the game's own latency. To get there:
- Process frames in or signalled from `FrameArrived`, always taking the newest frame and dropping stale ones.
- GPU-only critical path with no readback.
- Swapchain created with `FRAME_LATENCY_WAITABLE_OBJECT` and `SetMaximumFrameLatency(1)`.
- Pre-create all views.
- The engine thread waits with `MsgWaitForMultipleObjects` instead of busy-polling.

### Failure behavior (safety-relevant)
- If the target closes, the device is lost or capture errors: hide the overlay, show a notification and a tray icon state, then attempt a restart once.
- Exclusive fullscreen: if the target covers its monitor and capture yields black or no frames within ~1 s after the game draws, warn "switch to borderless".
- HDR monitor detected (`IDXGIOutput6` colorspace): warn "HDR not supported in this version".

---

## Part 3 — Phased implementation

### Phase 0 — Foundations (small, first PR)
- Restore `.github/workflows/ci.yml` on `windows-latest`:
  - `cargo clippy -D warnings`
  - `cargo test --workspace`
  - `npm ci && npm run build`
  - `tauri build` (`--no-bundle` until Phase 6)
- Generate real icons (`npx @tauri-apps/cli icon`) and remove the 0-byte schema files. Leave them to regenerate.
- Add `tracing` and `tracing-appender` with a daily log in `%APPDATA%\FlashSafe\logs`. Reuse the approach from the deleted `crates/flashsafe-app/src/logging.rs` (`git show HEAD~1:...`).
- Drop `tauri-plugin-shell` if it's unused, set a real CSP, and trim the README.
- Dev loop from Linux: `rustup target add x86_64-pc-windows-msvc && cargo check --target x86_64-pc-windows-msvc -p flashsafe`. `check` doesn't link, so the Windows code can be type-checked here.

### Phase 1 — Core algorithm + WCAG analyzer (`crates/flashsafe-core`, pure Rust, fully tested)
- `wcag.rs`: restore and clean up `LuminanceDetector` / `RedFlashDetector` from `HEAD~1`, using sRGB→linear and opposing-transition counting over a 1 s window. This is the *judge* used by tests and by the activity metric.
- `filter.rs`: `TileFilter` CPU reference (per-tile `L_disp`, activity, hold, gain), a `FilterParams` struct, and a `#[repr(C)]` constant-buffer layout shared with HLSL.
- `tiles.rs`: proper area-average tile luminance from BGRA, replacing the point-sampling `DownsampleStats`.
- Replace `PipelineSettings` with `FilterParams` plus presets, and add a `configVersion` migration in `config.rs`. Unknown or old fields fall back to defaults.
- Delete `FastFlashDetector`, the biquads and `MitigationParams`.
- Tests with synthetic sequences at 30, 60, 144 fps and irregular timestamps:
  - A 10 Hz full-screen white strobe: the output passes the WCAG analyzer (≤3 flashes/s).
  - Localized strobes in 30% of the frame.
  - A red strobe.
  - A single camera flash: peak output luminance change is bounded.
  - Normal gameplay (slow pan, gradual fade): gain stays ≈1, so there's no false dimming.
  - A scene cut to bright: a short, bounded ramp.
- Add a `flashsafe-sim` dev binary (in core, behind a feature flag) that runs the filter over a PNG sequence and prints the WCAG verdict before and after, for tuning presets.

### Phase 2 — Overlay + capture rewrite (`src-tauri/src/win/`)
Split `engine.rs` into:
- `capture.rs`: WGC session, cursor and border off, newest-frame, resize/Recreate, `Closed` handling.
- `overlay.rs`: window styles above, DComp swapchain, display affinity, show/hide/position.
- `tracker.rs`: WinEvent hooks for foreground, location and minimize, plus geometry and DPI math.
- `gpu.rs`: device, shaders, cached views, WARP option.
- `engine.rs`: orchestrator thread with `MsgWaitForMultipleObjects` and a command channel.

At the end of this phase, pass frames through unmodified and verify input and alignment:
- Upgrade `src-tauri/src/bin/flashsafe_harness.rs` into an **input test target**. It should:
  - show counters for mouse clicks and position, keys pressed, raw-input mouse deltas and XInput buttons;
  - draw a crosshair at the cursor position;
  - have a mode that strobes at a configurable frequency.
- This proves pass-through without needing a real game.

### Phase 3 — GPU filter
- Add HLSL in `src-tauri/src/shaders/`:
  - `tiles.hlsl` (compute: linearize + tile average);
  - `state.hlsl` (compute: the filter update, mirroring `filter.rs` 1:1, with state in ping-pong `R16G16B16A16_FLOAT` textures);
  - `apply.hlsl` (full-screen triangle: crop, linearize, multiply by bilinear gain, re-encode).
- Compile at build time with `fxc` in `build.rs`, or keep runtime `D3DCompile` but compile once at start.
- Stats: copy a 1×1 summary into a ring of 3 staging textures and `Map(D3D11_MAP_FLAG_DO_NOT_WAIT)`, so the critical path never blocks.
- **Golden test:** on a WARP D3D11 device (works headless in CI), run the shaders on synthetic frames and compare tile gains to the CPU `TileFilter` within tolerance.

### Phase 4 — Robustness and edge cases
- Handle target close, device removed (recreate), minimize, alt-tab, resize and DPI changes; the target moving to another monitor should keep working.
- Add the exclusive fullscreen and HDR detection and warnings described in Part 2.
- Add a watchdog that logs and surfaces errors. The failure policy is the one described in Part 2.

### Phase 5 — UX for non-technical friends (`src/main.js`, `src/style.css`, `lib.rs`)
- Rebuild the main screen around:
  - a game picker (process name + title, refreshed live);
  - a big Protect toggle;
  - a strength selector (Low/Med/High);
  - a status line ("Protecting — 2 flashes softened in the last minute");
  - warnings for fullscreen and HDR;
  - Advanced settings collapsed.
- Tray icon (Tauri tray) with a show/hide/protect toggle. Closing the window minimizes to tray while protection is running.
- A global hotkey (`tauri-plugin-global-shortcut`, default Ctrl+Alt+F) toggles protection *without stealing focus*.
- First-run disclaimer: "Not a medical device; reduces but cannot guarantee elimination of flashes; stop playing if you feel unwell."
- A "Test it" button that launches the harness in strobe mode.
- Remember the last game by exe name. Stretch goal: auto-start protection when that exe's window appears.

### Phase 6 — Release
- `bundle.active: true` with an NSIS target, version 0.1.0, and the WebView2 bootstrapper.
- `.github/workflows/release.yml`: on a `v*` tag, build, attach the installer to a GitHub Release, and generate release notes.
- README "For players" section: install, click past SmartScreen, use borderless mode, known limits.
- Update `docs/game-mode-matrix.md` with a tested-games table.

---

## Critical files
- Rewrite:
  - `src-tauri/src/win/engine.rs` → split into `capture.rs`, `overlay.rs`, `tracker.rs`, `gpu.rs`, `engine.rs`.
  - `src-tauri/src/shader.hlsl` → `src-tauri/src/shaders/*.hlsl`.
- Heavily modify: `crates/flashsafe-core/src/{config,detection,mitigation}.rs`.
- Add: `wcag.rs`, `filter.rs`, `tiles.rs`.
- Modify:
  - `src-tauri/src/lib.rs` (commands, tray, hotkey, logging);
  - `src-tauri/Cargo.toml` (windows features: `Win32_Graphics_DirectComposition`, `Win32_Graphics_Dwm`, `Win32_UI_Accessibility` for WinEvent hooks, `Win32_UI_Input_XboxController` for the harness, `Graphics_Capture` access);
  - `src-tauri/tauri.conf.json`;
  - `src/main.js`, `src/style.css`;
  - `src-tauri/src/bin/flashsafe_harness.rs`.
- Add: `.github/workflows/{ci,release}.yml`.

## Verification
1. **Anywhere:** `cargo test -p flashsafe-core`. The synthetic strobe suite must pass the WCAG judge, and the no-false-dimming cases must hold gain ≈1.
2. **Type-check Windows code from Linux:** `cargo check --target x86_64-pc-windows-msvc`.
3. **Windows CI:** clippy, tests, the WARP golden test (GPU filter vs CPU reference) and `tauri build`.
4. **Manual on Windows with the harness:**
   - Clicks, keys, raw mouse and XInput counters all register with protection ON.
   - The crosshair aligns with the real cursor, including at 125% and 150% DPI.
   - Strobe mode at 5, 10 and 20 Hz is visibly flattened.
   - Alt-tab hides the overlay, and resize and move track correctly.
5. **Manual in 2–3 real borderless games**, one with a controller:
   - input works and there's no noticeable added lag;
   - flashes are softened and normal scenes aren't dimmed.
   - Record the results in `docs/game-mode-matrix.md`.
6. **Release dry run:** tag `v0.1.0-rc1`, then install the produced NSIS installer on a clean Windows machine or VM.
