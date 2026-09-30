//! Minimal top-level window that periodically fills the client area with white then dark
//! so you can validate WGC + detection in FlashSafe without a real game.

#[cfg(not(windows))]
fn main() {
    eprintln!("flashsafe-harness is only supported on Windows.");
}

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    flashsafe_harness::run()
}

#[cfg(windows)]
mod flashsafe_harness {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
        KillTimer, PostQuitMessage, RegisterClassExW, SetTimer, TranslateMessage, CS_HREDRAW,
        CS_VREDRAW, MSG, SW_SHOW, SetWindowTextW, ShowWindow, WM_DESTROY, WM_TIMER,
        WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
    };

    static FLASH: AtomicBool = AtomicBool::new(false);

    pub fn run() -> windows::core::Result<()> {
        unsafe {
            let hmod = GetModuleHandleW(None)?;
            let class = w!("FlashSafeHarnessCls");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                hInstance: hmod.into(),
                lpszClassName: class,
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let hwnd = CreateWindowExW(
                Default::default(),
                class,
                w!("FlashSafe test harness"),
                WS_OVERLAPPEDWINDOW,
                200,
                200,
                900,
                600,
                None,
                None,
                Some(hmod.into()),
                None,
            )?;
            let _ = SetWindowTextW(hwnd, w!("FlashSafe test harness — flashes every 2s"));
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetTimer(Some(hwnd), 1, 2000, None);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TIMER => {
                let on = !FLASH.fetch_xor(true, Ordering::SeqCst);
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                let hdc = GetDC(Some(hwnd));
                if hdc.is_invalid() {
                    return LRESULT(0);
                }
                let color = if on {
                    COLORREF(0x00FFFFFF)
                } else {
                    COLORREF(0x00101010)
                };
                let brush = CreateSolidBrush(color);
                let _ = FillRect(hdc, &r, brush);
                let _ = DeleteObject(brush.into());
                let _ = ReleaseDC(Some(hwnd), hdc);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
