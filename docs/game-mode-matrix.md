# FlashSafe — game display mode matrix

FlashSafe captures **what the Windows compositor shows** for a chosen window (Windows.Graphics.Capture), similar to Game Bar or desktop recorders. That implies clear limits for fullscreen behavior.

| Mode | Typical capture | User guidance |
|------|-----------------|---------------|
| **Windowed** | Works | Only the client area is filtered; the real title bar stays visible and clickable. |
| **Borderless fullscreen** (“windowed fullscreen”) | Works | **Recommended** for games. Same as fullscreen for many engines but still composited. |
| **Exclusive fullscreen** | Often **black / stale / fails** | The game may bypass DWM; per-window capture is unreliable. Use borderless or windowed. |
| **HDR display, SDR capture path** | May clip highlights | Mitigation is tuned for SDR; very bright HDR content may look different than in-game. |

## Manual check

1. Run `cargo run -p flashsafe --bin flashsafe-harness`.
2. In FlashSafe, pick the harness window and start protection.
3. Filter: when the harness flashes white, the flash should fade in instead of popping, and **Flashes softened** should increase. Keys `1`–`5` strobe at 3–20 Hz; each should flatten into a steady, dim picture.
4. Input: click, scroll, type and move the mouse over the harness, and use a controller if you have one. Every counter in its panel should keep counting, and the red crosshair should sit under the real cursor.
5. Focus: alt-tab to another window. The overlay should drop behind it (other windows look normal) while still covering the harness. Minimize the harness: the overlay disappears. Restore it: protection resumes.
6. Close the harness: FlashSafe should report that protection stopped because the game window closed.

## Anti-cheat note

Compositor capture does **not** inject into the game process. Some titles or anti-cheats may still object to screen capture APIs; that is outside FlashSafe’s control.
