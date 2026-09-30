# FlashSafe — game display mode matrix

> **Known issue:** the mirror currently returns `HTTRANSPARENT` from `WM_NCHITTEST`. That only forwards clicks to windows owned by the *same thread*, so clicks don't reach the game. The fix is a `WS_EX_LAYERED | WS_EX_TRANSPARENT` overlay presented through DirectComposition; see Phase 2 in [mvp-plan.md](mvp-plan.md).

FlashSafe captures **what the Windows compositor shows** for a chosen window (Windows.Graphics.Capture), similar to Game Bar or desktop recorders. That implies clear limits for fullscreen behavior.

| Mode | Typical capture | User guidance |
|------|-----------------|---------------|
| **Windowed** | Works | Easiest to align the mirror; may see title bar in capture depending on the title. |
| **Borderless fullscreen** (“windowed fullscreen”) | Works | **Recommended** for games. Same as fullscreen for many engines but still composited. |
| **Exclusive fullscreen** | Often **black / stale / fails** | The game may bypass DWM; per-window capture is unreliable. Use borderless or windowed. |
| **HDR display, SDR capture path** | May clip highlights | Mitigation is tuned for SDR; very bright HDR content may look different than in-game. |

## Manual check

1. Build and run `flashsafe-harness` (see crate `flashsafe`, binary `flashsafe-harness`).
2. In FlashSafe, pick the harness window and start protection.
3. The mirror should follow the window. When the harness flashes white, the flash should fade in instead of popping, and **Flashes softened** should increase.

## Anti-cheat note

Compositor capture does **not** inject into the game process. Some titles or anti-cheats may still object to screen capture APIs; that is outside FlashSafe’s control.
