//! The window FlashSafe draws the filtered game into.
//!
//! It must never get in the way of the game:
//! - **Input passes through.** `WS_EX_LAYERED | WS_EX_TRANSPARENT` makes the
//!   window invisible to hit-testing across processes, so clicks and mouse
//!   movement go to the game underneath. (`HTTRANSPARENT` from
//!   `WM_NCHITTEST`, used before, only forwards to windows of the same
//!   thread.)
//! - **Focus stays with the game.** `WS_EX_NOACTIVATE`, show/position calls
//!   with `SWP_NOACTIVATE` / `SW_SHOWNA`, and `MA_NOACTIVATE` mean the overlay
//!   is never the foreground window, so keyboard, raw input and XInput
//!   (which only reach the foreground window) keep working.
//! - **Z-order follows the game.** Topmost while the game is in front; when
//!   the user switches away it drops to just above the game, so other apps
//!   (including FlashSafe's own window) show normally. Hidden while the game
//!   is minimized.
//!
//! Presentation goes through DirectComposition
//! (`CreateSwapChainForComposition`), the supported way to put flip-model
//! D3D content on a layered, input-transparent window.

use anyhow::{Context, Result};
use windows::core::{w, Interface};
use windows::Win32::Foundation::{CloseHandle, COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11RenderTargetView, ID3D11Texture2D};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, IDXGISwapChain2, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetClientRect, GetForegroundWindow,
    GetWindow, IsIconic, IsWindowVisible, RegisterClassExW, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, GA_ROOTOWNER, GW_HWNDNEXT, GW_HWNDPREV,
    HTTRANSPARENT, HWND_NOTOPMOST, HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, WDA_EXCLUDEFROMCAPTURE, WM_MOUSEACTIVATE, WM_NCHITTEST, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

const SWAP_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

unsafe extern "system" fn overlay_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Belt and braces: WS_EX_TRANSPARENT already keeps hit-tests away.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Where and how the overlay should be shown this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Placement {
    Hidden,
    /// Game in front: overlay topmost.
    Topmost(RECT),
    /// Game behind something: overlay directly above the game.
    AboveGame(RECT),
}

pub struct Overlay {
    hwnd: HWND,
    swap: IDXGISwapChain2,
    rtv: Option<ID3D11RenderTargetView>,
    size: (u32, u32),
    waitable: HANDLE,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    placement: Placement,
    /// Nothing is shown until the first frame has been presented.
    has_content: bool,
}

impl Overlay {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let hwnd = create_window()?;
        let dxgi: IDXGIDevice = device.cast().context("IDXGIDevice")?;
        let factory: IDXGIFactory2 = unsafe { dxgi.GetAdapter()?.GetParent()? };
        let size = (width.max(1), height.max(1));
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0,
            Height: size.1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: SWAP_FLAGS.0 as u32,
        };
        let swap: IDXGISwapChain1 = unsafe { factory.CreateSwapChainForComposition(device, &desc, None) }
            .context("CreateSwapChainForComposition")?;
        let swap: IDXGISwapChain2 = swap.cast().context("IDXGISwapChain2")?;
        // One frame in flight: we wait for it before drawing, so the mirror
        // never queues up stale frames.
        unsafe { swap.SetMaximumFrameLatency(1)? };
        let waitable = unsafe { swap.GetFrameLatencyWaitableObject() };

        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi) }.context("DCompositionCreateDevice")?;
        let target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true) }.context("CreateTargetForHwnd")?;
        let visual = unsafe { dcomp.CreateVisual() }.context("CreateVisual")?;
        unsafe {
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;
        }

        let mut overlay = Self {
            hwnd,
            swap,
            rtv: None,
            size,
            waitable,
            _dcomp: dcomp,
            _target: target,
            _visual: visual,
            placement: Placement::Hidden,
            has_content: false,
        };
        overlay.rtv = Some(overlay.back_buffer_rtv(device)?);
        Ok(overlay)
    }

    fn back_buffer_rtv(&self, device: &ID3D11Device) -> Result<ID3D11RenderTargetView> {
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv))? };
        rtv.context("overlay RTV")
    }

    /// Wait until the swapchain can take a frame, resize it if needed, and
    /// return the render target to draw this frame into.
    pub fn begin_frame(
        &mut self,
        device: &ID3D11Device,
        flush: impl FnOnce(),
        width: u32,
        height: u32,
    ) -> Result<ID3D11RenderTargetView> {
        unsafe {
            WaitForSingleObjectEx(self.waitable, 100, true);
        }
        if self.size != (width, height) {
            // Every reference to the back buffer must go before ResizeBuffers.
            self.rtv = None;
            flush();
            unsafe {
                self.swap
                    .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, SWAP_FLAGS)
                    .context("ResizeBuffers")?;
            }
            self.size = (width, height);
            self.rtv = Some(self.back_buffer_rtv(device)?);
        }
        self.rtv.clone().context("overlay RTV")
    }

    pub fn present(&mut self) {
        unsafe {
            // Sync interval 0: DWM composes the latest frame; nothing waits on vsync here.
            let _ = self.swap.Present(0, DXGI_PRESENT(0));
        }
        self.has_content = true;
    }

    /// Match the overlay to the game window: cover its client area, keep it
    /// just above the game in z-order, and hide it while the game is minimized.
    /// Cheap enough to call every engine tick; only calls `SetWindowPos` when
    /// something changed.
    pub fn follow(&mut self, game: HWND) {
        let want = if !self.has_content {
            Placement::Hidden
        } else {
            match game_client_rect(game) {
                None => Placement::Hidden,
                Some(r) if game_is_foreground(game) => Placement::Topmost(r),
                Some(r) => Placement::AboveGame(r),
            }
        };
        let above_game_ok = || unsafe { GetWindow(self.hwnd, GW_HWNDNEXT).ok() == Some(game) };
        if want == self.placement && (!matches!(want, Placement::AboveGame(_)) || above_game_ok()) {
            return;
        }
        unsafe {
            match want {
                Placement::Hidden => {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                Placement::Topmost(r) => {
                    let _ = SetWindowPos(
                        self.hwnd,
                        Some(HWND_TOPMOST),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
                Placement::AboveGame(r) => {
                    // Inserting after the window directly above the game puts
                    // us between the two. If nothing is above it, top of the
                    // non-topmost band. Either way we lose topmost status.
                    let above = GetWindow(game, GW_HWNDPREV).ok().filter(|h| *h != self.hwnd);
                    let _ = SetWindowPos(
                        self.hwnd,
                        Some(above.unwrap_or(HWND_NOTOPMOST)),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
            }
        }
        self.placement = want;
    }

}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.rtv = None;
        unsafe {
            let _ = CloseHandle(self.waitable);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn create_window() -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).context("GetModuleHandleW")?;
        let class = w!("FlashSafeOverlay");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(overlay_wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Fails harmlessly if the class is already registered (a restart).
        let _ = RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP
                | WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOPMOST
                | WS_EX_NOACTIVATE
                | WS_EX_TOOLWINDOW,
            class,
            w!("FlashSafe overlay"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .context("CreateWindowExW")?;
        // A layered window is only drawn once it has attributes; fully opaque.
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA).context("SetLayeredWindowAttributes")?;
        // Keep the filtered picture out of screenshots / screen recordings of
        // the whole desktop (so it never feeds back into a capture). Needs
        // Windows 10 2004+; older builds just ignore it.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        Ok(hwnd)
    }
}

/// The game's client area in screen coordinates, or `None` when there is
/// nothing to cover (minimized, hidden, zero-size).
pub fn game_client_rect(game: HWND) -> Option<RECT> {
    unsafe {
        if IsIconic(game).as_bool() || !IsWindowVisible(game).as_bool() {
            return None;
        }
        let mut client = RECT::default();
        GetClientRect(game, &mut client).ok()?;
        if client.right <= 0 || client.bottom <= 0 {
            return None;
        }
        let mut origin = POINT::default();
        if !ClientToScreen(game, &mut origin).as_bool() {
            return None;
        }
        Some(RECT {
            left: origin.x,
            top: origin.y,
            right: origin.x + client.right,
            bottom: origin.y + client.bottom,
        })
    }
}

/// True when the game (or a window it owns, like a dialog) is in front.
fn game_is_foreground(game: HWND) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        !fg.is_invalid() && (fg == game || GetAncestor(fg, GA_ROOTOWNER) == game)
    }
}
