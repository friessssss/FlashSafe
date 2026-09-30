# FlashSafe

Windows app that captures a chosen window via **Windows.Graphics.Capture**, detects sudden bright flashes, and shows a **filtered mirror** aligned on top of the game. Works with games using DirectX, Vulkan, or OpenGL because capture is compositor-based (same idea as Game Bar / desktop recording)—not API injection.

**Important:** Use **borderless windowed** (or windowed) fullscreen. True exclusive fullscreen often cannot be captured.

## Prerequisites

- **Windows 10/11**
- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) 18+ and npm (for the Tauri frontend). On Windows, install Node from the website or a manager you actually have on your PATH (`nvm` in PowerShell is often missing unless you installed [nvm-windows](https://github.com/coreybutler/nvm-windows) or use `fnm` / `volta`).
- **Visual Studio Build Tools** with **C++** workload and **Windows SDK** (required for Tauri and the `windows` crate)

Install the Tauri CLI (pick one):

```powershell
cargo install tauri-cli --locked
# or use npx without a global install (see below)
```

## Build & run (development)

From the repository root:

```powershell
npm install
```

Start the Vite dev server and the desktop app (Tauri runs `npm run dev` automatically):

```powershell
cargo tauri dev
```

If you do not have the CLI installed globally:

```powershell
npx --yes @tauri-apps/cli@2 dev
```

## Production build

Build the web assets, then compile the app:

```powershell
npm run build
cargo tauri build
```

With `npx`:

```powershell
npm run build
npx --yes @tauri-apps/cli@2 build
```

Release output is under `src-tauri/target/release/`. Bundling is currently **disabled** in `tauri.conf.json` (`bundle.active: false`); enable it there when you want an installer.

## Troubleshooting: `npm` in Cursor’s terminal vs normal PowerShell

The integrated terminal does **not** clone a random PowerShell window. It starts from **Cursor’s own process environment** (fixed when Cursor was launched). External PowerShell often runs **`$PROFILE`** scripts (fnm, Volta, conda, custom PATH tweaks) that Cursor never applies the same way, so `npm` can work “in normal PowerShell” but not inside Cursor.

**Try, in order:**

1. **Quit Cursor completely** (all windows), then open it again. Installers that update your **User** `Path` only affect newly started apps.
2. Compare: run `where.exe npm` in external PowerShell and in Cursor’s terminal. If only the first finds it, add that folder to **User** `Path` in Windows: *Settings → System → About → Advanced system settings → Environment Variables → Path (User)*. The folder must contain `npm.cmd` (often `C:\Program Files\nodejs`, or nvm-windows’ active version path).
3. **Start Cursor from the shell where `npm` works** so it inherits that PATH, e.g. `cursor "C:\Users\you\Desktop\Apps\Coding\FlashSafe"`.
4. Optional workspace override: create `.vscode/settings.json` and prepend your Node directory (adjust the path to match `where.exe npm`):

```json
{
  "terminal.integrated.env.windows": {
    "Path": "C:\\Program Files\\nodejs;${env:Path}"
  }
}
```

## Optional: test harness

A small window that flashes every few seconds so you can verify capture and detection without a game:

```powershell
cargo run -p flashsafe --bin flashsafe-harness
```

Then in FlashSafe, refresh the window list, pick **“FlashSafe test harness”**, and start protection.

## Tests

```powershell
cargo test -p flashsafe-core
```

## Docs

- [Game / display mode notes](docs/game-mode-matrix.md)
- [ADR: stack](docs/adr/001-tech-stack.md)

## Settings

Saved to `%APPDATA%\FlashSafe\settings.json`.
