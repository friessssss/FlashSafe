//! Test window for FlashSafe: flashes on demand and shows every kind of
//! input it receives, so you can check both the filter and input
//! pass-through without a real game.
//!
//! Keys (while the harness has focus):
//! - `0` slow toggle every 2 s (default), `1`–`5` strobe at 3 / 5 / 10 / 15 / 20 Hz
//! - `D` stay dark, `W` stay white
//!
//! The panel shows mouse clicks and position, wheel, key presses, raw mouse
//! deltas (what most games read) and XInput controller state. With
//! protection on, all of them should keep counting — and the red crosshair
//! should sit exactly under the real cursor.

#[cfg(not(windows))]
fn main() {
    eprintln!("flashsafe-harness is only supported on Windows.");
}

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    harness::run()
}

#[cfg(windows)]
mod harness {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
        DeleteDC, DeleteObject, EndPaint, FillRect, InvalidateRect, LineTo, MoveToEx, SelectObject,
        SetBkMode, SetTextColor, TextOutW, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};
    use windows::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
        RID_INPUT, RIM_TYPEMOUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, KillTimer,
        PostQuitMessage, RegisterClassExW, SetTimer, SetWindowTextW, ShowWindow, TranslateMessage,
        CS_HREDRAW, CS_VREDRAW, MSG, SW_SHOW, WM_DESTROY, WM_ERASEBKGND, WM_INPUT, WM_KEYDOWN,
        WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN,
        WM_SYSKEYDOWN, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
    };

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// Toggle every 2 s (the original harness behaviour).
        Slow,
        Strobe(f64),
        Dark,
        White,
    }

    impl Mode {
        fn label(self) -> String {
            match self {
                Mode::Slow => "toggles every 2 s".into(),
                Mode::Strobe(hz) => format!("strobing at {hz} Hz"),
                Mode::Dark => "dark".into(),
                Mode::White => "white".into(),
            }
        }

        fn is_white(self, t: f64) -> bool {
            match self {
                Mode::Slow => ((t / 2.0).floor() as i64) % 2 == 1,
                Mode::Strobe(hz) => ((t * hz * 2.0).floor() as i64) % 2 == 0,
                Mode::Dark => false,
                Mode::White => true,
            }
        }
    }

    struct State {
        mode: Mode,
        mouse: (i32, i32),
        clicks: [u32; 3],
        wheel: i32,
        keys: u32,
        last_key: u32,
        raw: (i64, i64),
        raw_events: u32,
        pad: Option<XINPUT_STATE>,
    }

    static STATE: Mutex<State> = Mutex::new(State {
        mode: Mode::Slow,
        mouse: (-100, -100),
        clicks: [0; 3],
        wheel: 0,
        keys: 0,
        last_key: 0,
        raw: (0, 0),
        raw_events: 0,
        pad: None,
    });
    static START: OnceLock<Instant> = OnceLock::new();

    fn elapsed() -> f64 {
        START.get_or_init(Instant::now).elapsed().as_secs_f64()
    }

    fn title(mode: Mode) -> Vec<u16> {
        format!("FlashSafe test harness — {} (keys 0-5, D, W)\0", mode.label())
            .encode_utf16()
            .collect()
    }

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
            let t = title(Mode::Slow);
            let hwnd = CreateWindowExW(
                Default::default(),
                class,
                PCWSTR(t.as_ptr()),
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
            // Raw mouse input, like most games use. Only delivered while the
            // harness is the foreground window, same as for a game.
            let rid = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: Default::default(),
                hwndTarget: hwnd,
            };
            RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32)?;
            let _ = ShowWindow(hwnd, SW_SHOW);
            elapsed();
            // ~100 Hz redraw: fast enough to show a 20 Hz strobe cleanly.
            let _ = SetTimer(Some(hwnd), 1, 10, None);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    fn lparam_xy(l: LPARAM) -> (i32, i32) {
        ((l.0 & 0xffff) as i16 as i32, ((l.0 >> 16) & 0xffff) as i16 as i32)
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_TIMER => {
                let mut pad = XINPUT_STATE::default();
                let connected = XInputGetState(0, &mut pad) == 0;
                STATE.lock().unwrap().pad = connected.then_some(pad);
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                STATE.lock().unwrap().mouse = lparam_xy(lparam);
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                let i = match msg {
                    WM_LBUTTONDOWN => 0,
                    WM_RBUTTONDOWN => 1,
                    _ => 2,
                };
                let mut s = STATE.lock().unwrap();
                s.clicks[i] += 1;
                s.mouse = lparam_xy(lparam);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                STATE.lock().unwrap().wheel += ((wparam.0 >> 16) & 0xffff) as i16 as i32 / 120;
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wparam.0 as u32;
                let new_mode = match vk {
                    0x30 => Some(Mode::Slow),
                    0x31 => Some(Mode::Strobe(3.0)),
                    0x32 => Some(Mode::Strobe(5.0)),
                    0x33 => Some(Mode::Strobe(10.0)),
                    0x34 => Some(Mode::Strobe(15.0)),
                    0x35 => Some(Mode::Strobe(20.0)),
                    0x44 => Some(Mode::Dark),
                    0x57 => Some(Mode::White),
                    _ => None,
                };
                {
                    let mut s = STATE.lock().unwrap();
                    s.keys += 1;
                    s.last_key = vk;
                    if let Some(m) = new_mode {
                        s.mode = m;
                    }
                }
                if let Some(m) = new_mode {
                    let t = title(m);
                    let _ = SetWindowTextW(hwnd, PCWSTR(t.as_ptr()));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_INPUT => {
                let mut raw = RAWINPUT::default();
                let mut size = std::mem::size_of::<RAWINPUT>() as u32;
                let n = GetRawInputData(
                    HRAWINPUT(lparam.0 as *mut _),
                    RID_INPUT,
                    Some(&mut raw as *mut RAWINPUT as *mut _),
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                );
                if n != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
                    let m = raw.data.mouse;
                    let mut s = STATE.lock().unwrap();
                    s.raw.0 += m.lLastX as i64;
                    s.raw.1 += m.lLastY as i64;
                    s.raw_events += 1;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn paint(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let (w, h) = (r.right.max(1), r.bottom.max(1));

        // Draw into a back buffer so the strobe has clean edges.
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp.into());

        let s = STATE.lock().unwrap();
        let bg = if s.mode.is_white(elapsed()) { 0x00FF_FFFF } else { 0x0010_1010 };
        fill(mem, &r, bg);

        // Static info panel — also a handy "HUD next to a flash" for the filter.
        let panel = RECT { left: 16, top: 16, right: 470, bottom: 262 };
        fill(mem, &panel, 0x0030_3030);
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, COLORREF(0x00E0_E0E0));
        let pad = match s.pad {
            Some(p) => format!(
                "Controller: buttons {:#06x}  LT {}  RT {}  L ({}, {})",
                p.Gamepad.wButtons.0, p.Gamepad.bLeftTrigger, p.Gamepad.bRightTrigger,
                p.Gamepad.sThumbLX, p.Gamepad.sThumbLY
            ),
            None => "Controller: not connected".into(),
        };
        let lines = [
            format!("Mode: {}   (keys 0-5, D, W)", s.mode.label()),
            format!("Mouse: ({}, {})", s.mouse.0, s.mouse.1),
            format!("Clicks: left {}  right {}  middle {}", s.clicks[0], s.clicks[1], s.clicks[2]),
            format!("Wheel: {}", s.wheel),
            format!("Keys pressed: {}   last VK {:#04x}", s.keys, s.last_key),
            format!("Raw mouse: ({}, {}) from {} events", s.raw.0, s.raw.1, s.raw_events),
            pad,
            "If these stop counting with protection on,".into(),
            "input is not reaching the game.".into(),
        ];
        for (i, line) in lines.iter().enumerate() {
            let text: Vec<u16> = line.encode_utf16().collect();
            let _ = TextOutW(mem, 28, 26 + i as i32 * 25, &text);
        }

        // Crosshair where the harness thinks the cursor is.
        let pen = CreatePen(PS_SOLID, 2, COLORREF(0x0000_00FF));
        let old_pen = SelectObject(mem, pen.into());
        let (mx, my) = s.mouse;
        let _ = MoveToEx(mem, mx - 14, my, None);
        let _ = LineTo(mem, mx + 15, my);
        let _ = MoveToEx(mem, mx, my - 14, None);
        let _ = LineTo(mem, mx, my + 15);
        SelectObject(mem, old_pen);
        let _ = DeleteObject(pen.into());
        drop(s);

        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }

    unsafe fn fill(hdc: windows::Win32::Graphics::Gdi::HDC, r: &RECT, color: u32) {
        let brush = CreateSolidBrush(COLORREF(color));
        let _ = FillRect(hdc, r, brush);
        let _ = DeleteObject(brush.into());
    }
}
