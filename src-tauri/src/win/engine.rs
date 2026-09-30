//! WGC → client-area crop → per-pixel stats → [`TileFilter`] scales → per-pixel filter shader → mirror `HWND`.
//!
//! Interim engine: the per-tile budget runs on the CPU from a small readback
//! (per-pixel work is on the GPU), and the mirror still uses an HWND swapchain. See `docs/mvp-plan.md` (Phases 2–3)
//! for the click-through DirectComposition overlay and GPU filter that
//! replace this.

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flashsafe_core::{
    tile_stats_f32x4, FlashSafeConfig, TileFilter, TileStats, TILE_COLS, TILE_ROWS,
};
use parking_lot::RwLock;
use serde::Serialize;
use windows::core::{BOOL, IInspectable, Interface, s};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::UI::WindowId;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CULL_NONE, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_RASTERIZER_DESC, D3D11_RESOURCE_MISC_GENERATE_MIPS,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32G32B32A32_FLOAT,
    DXGI_FORMAT_R32G32_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GetClientRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, PeekMessageW, PostQuitMessage, RegisterClassExW, SetWindowPos, ShowWindow,
    TranslateMessage, HTTRANSPARENT, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOWNA,
    HWND_TOPMOST, WM_DESTROY, WM_NCCREATE, WM_NCHITTEST, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOPMOST, WS_POPUP,
};

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStats {
    pub running: bool,
    pub frames: u64,
    pub fps: f32,
    /// Distinct mitigation episodes ("flashes softened") this session.
    pub flashes: u64,
    /// Strongest dimming currently applied (1 − min gain), 0–1.
    pub mitigation: f32,
    /// Fraction of the screen in strobe hold, 0–1.
    pub hold: f32,
    pub last_error: Option<String>,
}

pub enum EngineCommand {
    Start { hwnd: isize, config: FlashSafeConfig },
    Stop,
    UpdateConfig(FlashSafeConfig),
}

pub fn enumerate_capture_windows() -> Vec<(u64, String, u32)> {
    let mut out: Vec<(u64, String, u32)> = Vec::new();
    unsafe extern "system" fn cb(hwnd: HWND, l: LPARAM) -> BOOL {
        let out = &mut *(l.0 as *mut Vec<(u64, String, u32)>);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if title.trim().is_empty() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        out.push((hwnd.0 as usize as u64, title, pid));
        BOOL(1)
    }
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

pub fn spawn_engine_thread() -> (Sender<EngineCommand>, Arc<RwLock<EngineStats>>) {
    let stats = Arc::new(RwLock::new(EngineStats::default()));
    let st = Arc::clone(&stats);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("flashsafe-engine".into())
        .spawn(move || {
            let _ = engine_main(rx, st);
        })
        .expect("engine thread");
    (tx, stats)
}

fn engine_main(rx: Receiver<EngineCommand>, stats: Arc<RwLock<EngineStats>>) -> Result<()> {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }

    let mut session: Option<ActiveSession> = None;
    let mut fps_tick = Instant::now();
    let mut fps_frames = 0u32;

    loop {
        // Block briefly for commands only when idle; while running, the
        // session's own frame wait paces the loop.
        let cmd = if session.is_some() {
            rx.try_recv().map_err(|e| match e {
                TryRecvError::Empty => RecvTimeoutError::Timeout,
                TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else {
            rx.recv_timeout(Duration::from_millis(50))
        };
        match cmd {
            Ok(EngineCommand::Stop) => {
                if session.take().is_some() {
                    tracing::info!("protection stopped");
                }
                let mut s = stats.write();
                s.running = false;
                s.mitigation = 0.0;
                s.hold = 0.0;
            }
            Ok(EngineCommand::UpdateConfig(c)) => {
                if let Some(ref mut s) = session {
                    s.set_config(c);
                }
            }
            Ok(EngineCommand::Start { hwnd, config }) => {
                session = None;
                let mut s = stats.write();
                *s = EngineStats::default();
                match ActiveSession::new(HWND(hwnd as *mut c_void), config) {
                    Ok(sess) => {
                        tracing::info!(preset = ?sess.config.sensitivity_preset, "protection started");
                        session = Some(sess);
                        s.running = true;
                    }
                    Err(e) => {
                        tracing::warn!("start failed: {e:#}");
                        s.last_error = Some(format!(
                            "Cannot capture this window — use borderless/windowed mode. {e:#}"
                        ));
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }

        if let Some(ref mut sess) = session {
            match sess.tick() {
                Ok(presented) => {
                    let sum = sess.filter.summary();
                    let mut s = stats.write();
                    s.mitigation = 1.0 - sum.min_gain;
                    s.hold = sum.hold_fraction;
                    s.flashes = sum.events;
                    if presented {
                        s.frames = s.frames.saturating_add(1);
                        fps_frames += 1;
                    }
                    if fps_tick.elapsed() >= Duration::from_millis(500) {
                        s.fps = fps_frames as f32 / fps_tick.elapsed().as_secs_f32().max(0.001);
                        fps_frames = 0;
                        fps_tick = Instant::now();
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let mut s = stats.write();
                    if s.last_error.as_deref() != Some(msg.as_str()) {
                        tracing::error!("engine tick failed: {msg}");
                    }
                    s.last_error = Some(msg);
                }
            }
        }
        pump_messages();
    }
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_DESTROY {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn create_capture_item(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    let id = WindowId {
        Value: hwnd.0 as usize as u64,
    };
    GraphicsCaptureItem::TryCreateFromWindowId(id).map_err(|e| anyhow::anyhow!("{e:?}"))
}

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut ctx = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .ok()
        .context("D3D11CreateDevice")?;
    }
    Ok((device.context("device")?, ctx.context("context")?))
}

fn create_winrt_device(d3d: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = d3d.cast().context("IDXGIDevice")?;
    let insp = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    let dev: IDirect3DDevice = insp.cast().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(dev)
}

/// Compiled shader set: one vertex shader (full-screen triangle) and the
/// pixel shaders of each pipeline pass.
struct Shaders {
    vs: ID3D11VertexShader,
    prime: ID3D11PixelShader,
    stats: ID3D11PixelShader,
    downsample: ID3D11PixelShader,
    apply: ID3D11PixelShader,
}

fn compile_blob(src: &str, entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<Vec<u8>> {
    let mut blob = None;
    let mut errors = None;
    let res = unsafe {
        D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            &mut blob,
            Some(&mut errors),
        )
    };
    if let Err(e) = res {
        let msg = errors
            .map(|b: windows::Win32::Graphics::Direct3D::ID3DBlob| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        anyhow::bail!("shader compile failed: {e} {msg}");
    }
    let blob = blob.context("shader blob")?;
    Ok(unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }.to_vec())
}

fn compile_shaders(device: &ID3D11Device, src: &str) -> Result<Shaders> {
    let vs_code = compile_blob(src, s!("VSMain"), s!("vs_5_0"))?;
    let mut vs = None;
    unsafe { device.CreateVertexShader(&vs_code, None, Some(&mut vs))? };
    let ps = |entry| -> Result<ID3D11PixelShader> {
        let code = compile_blob(src, entry, s!("ps_5_0"))?;
        let mut ps = None;
        unsafe { device.CreatePixelShader(&code, None, Some(&mut ps))? };
        ps.context("pixel shader")
    };
    Ok(Shaders {
        vs: vs.context("vertex shader")?,
        prime: ps(s!("PSPrime"))?,
        stats: ps(s!("PSStats"))?,
        downsample: ps(s!("PSDownsample"))?,
        apply: ps(s!("PSMain"))?,
    })
}

static MIRROR_CLASS: &[u16] = &[
    b'F' as u16, b'l' as u16, b'a' as u16, b's' as u16, b'h' as u16, b'S' as u16, b'a' as u16,
    b'f' as u16, b'e' as u16, b'M' as u16, 0,
];

unsafe extern "system" fn mirror_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => LRESULT(1),
        // Pass mouse hits to the game below (more reliable with D3D flip swapchains than WS_EX_TRANSPARENT).
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_mirror_window(w: i32, h: i32) -> Result<HWND> {
    unsafe {
        let hmod = GetModuleHandleW(None).unwrap_or_default();
        let hinst = HINSTANCE(hmod.0);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: windows::Win32::UI::WindowsAndMessaging::CS_HREDRAW
                | windows::Win32::UI::WindowsAndMessaging::CS_VREDRAW,
            lpfnWndProc: Some(mirror_wnd_proc),
            hInstance: hinst,
            lpszClassName: windows::core::PCWSTR(MIRROR_CLASS.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        // Click-through via WM_NCHITTEST → HTTRANSPARENT (see mirror_wnd_proc).
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            windows::core::PCWSTR(MIRROR_CLASS.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_POPUP,
            200,
            200,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        Ok(hwnd)
    }
}

fn create_swapchain(
    device: &ID3D11Device,
    hwnd: HWND,
    w: u32,
    h: u32,
) -> Result<(IDXGISwapChain1, ID3D11RenderTargetView)> {
    let dxgi: IDXGIDevice = device.cast().unwrap();
    let adapter = unsafe { dxgi.GetAdapter().ok().context("adapter")? };
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent().context("factory")? };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w,
        Height: h,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let sc = unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None)? };
    let back: ID3D11Texture2D = unsafe { sc.GetBuffer(0)? };
    let mut rtv = None;
    unsafe {
        device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
    }
    Ok((sc, rtv.context("rtv")?))
}

/// Where the target's client area is: on screen, and inside the captured
/// texture. WGC captures the window's visible frame (DWM extended frame
/// bounds, title bar included); we only filter and cover the client area, so
/// the real title bar and borders stay visible and clickable.
struct ClientArea {
    screen: RECT,
    /// Crop box inside the captured texture.
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

fn client_area(target: HWND, content_w: u32, content_h: u32) -> Option<ClientArea> {
    unsafe {
        let mut frame = RECT::default();
        DwmGetWindowAttribute(
            target,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
        let mut client = RECT::default();
        GetClientRect(target, &mut client).ok()?;
        let mut origin = POINT::default();
        if !ClientToScreen(target, &mut origin).as_bool() {
            return None;
        }
        let left = (origin.x - frame.left).clamp(0, content_w as i32) as u32;
        let top = (origin.y - frame.top).clamp(0, content_h as i32) as u32;
        let width = (client.right as u32).min(content_w - left);
        let height = (client.bottom as u32).min(content_h - top);
        if width == 0 || height == 0 {
            return None;
        }
        Some(ClientArea {
            screen: RECT {
                left: origin.x,
                top: origin.y,
                right: origin.x + width as i32,
                bottom: origin.y + height as i32,
            },
            left,
            top,
            width,
            height,
        })
    }
}

/// Only `SetWindowPos` when the client area moves/resizes — avoids redundant DWM work every frame.
fn sync_mirror_if_moved(mirror: HWND, r: RECT, last: &mut Option<RECT>) {
    if last.as_ref() == Some(&r) {
        return;
    }
    *last = Some(r);
    unsafe {
        let _ = SetWindowPos(
            mirror,
            Some(HWND_TOPMOST),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// Resolution of the stats image read back each frame (RGBA32F). 5×5
/// texels per tile; each texel is a true area average via the mip chain.
const DETECTION_W: u32 = TILE_COLS as u32 * 5;
const DETECTION_H: u32 = TILE_ROWS as u32 * 5;
/// While the filter is still ramping, re-present the last frame at this
/// interval even if the game hasn't drawn a new one.
const REPRESENT_INTERVAL: Duration = Duration::from_millis(16);

/// A texture with the views we need on it.
struct Target {
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
}

impl Target {
    fn new(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, mips: bool) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: if mips { 0 } else { 1 },
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: if mips { D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32 } else { 0 },
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.context("CreateTexture2D")?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut rtv))? };
        Ok(Self {
            srv: create_srv(device, &tex)?,
            rtv: rtv.context("rtv")?,
            tex,
        })
    }
}

/// Everything sized to the target's client area; rebuilt when it resizes.
struct Surfaces {
    width: u32,
    height: u32,
    /// Latest captured client area (BGRA8, sRGB-encoded).
    frame: ID3D11Texture2D,
    frame_srv: ID3D11ShaderResourceView,
    /// Displayed colour, linear RGB, ping-ponged: `hist[cur]` is last frame.
    hist: [Target; 2],
    cur: usize,
    primed: bool,
    /// Per-pixel (L, P, rise, fall) with a mip chain for area averages.
    stats: Target,
    stats_level: f32,
}

impl Surfaces {
    fn new(device: &ID3D11Device, w: u32, h: u32) -> Result<Self> {
        let frame = create_texture(
            device,
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
        )?;
        // Deepest mip whose width still covers the detection image, so the
        // bilinear downsample reads true area averages.
        let mut level = 0u32;
        while (w >> (level + 1)) >= DETECTION_W && (h >> (level + 1)) >= DETECTION_H {
            level += 1;
        }
        Ok(Self {
            width: w,
            height: h,
            frame_srv: create_srv(device, &frame)?,
            frame,
            hist: [
                Target::new(device, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, false)?,
                Target::new(device, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, false)?,
            ],
            cur: 0,
            primed: false,
            stats: Target::new(device, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, true)?,
            stats_level: level as f32,
        })
    }
}

/// Mirrors `cbuffer Params` in shader.hlsl.
#[repr(C)]
#[derive(Clone, Copy)]
struct ShaderParams {
    min_gain: f32,
    stats_level: f32,
    _pad: [f32; 2],
}

struct ActiveSession {
    target_hwnd: HWND,
    mirror_hwnd: HWND,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    swap_rtv: Option<ID3D11RenderTargetView>,
    swap_size: (u32, u32),
    shaders: Shaders,
    samp: ID3D11SamplerState,
    rs: ID3D11RasterizerState,
    cb: ID3D11Buffer,
    det: Target,
    det_staging: ID3D11Texture2D,
    scale_tex: ID3D11Texture2D,
    scale_srv: ID3D11ShaderResourceView,
    last_client_rect: Option<RECT>,
    _pool_cell: Arc<Mutex<Option<Direct3D11CaptureFramePool>>>,
    pool: Direct3D11CaptureFramePool,
    pool_size: SizeInt32,
    winrt_device: IDirect3DDevice,
    _session: GraphicsCaptureSession,
    frame_rx: Receiver<Direct3D11CaptureFrame>,
    config: FlashSafeConfig,
    filter: TileFilter,
    stats: Vec<TileStats>,
    scales: Vec<f32>,
    surfaces: Option<Surfaces>,
    /// `SystemRelativeTime` of the last captured frame (100 ns units).
    last_capture_time: Option<i64>,
    last_present: Instant,
}

impl ActiveSession {
    fn new(target_hwnd: HWND, config: FlashSafeConfig) -> Result<Self> {
        if !unsafe { IsWindow(Some(target_hwnd)).as_bool() } {
            anyhow::bail!("Invalid HWND");
        }
        let item = create_capture_item(target_hwnd)?;
        let size = item.Size().context("item size")?;
        let (device, ctx) = create_d3d11_device()?;
        let winrt_device = create_winrt_device(&device)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .context("frame pool")?;

        let pool_cell: Arc<Mutex<Option<Direct3D11CaptureFramePool>>> = Arc::new(Mutex::new(None));
        let (tx, frame_rx) = mpsc::sync_channel(2);
        let cell = Arc::clone(&pool_cell);
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_pool, _args| {
                if let Ok(g) = cell.lock() {
                    if let Some(ref p) = *g {
                        if let Ok(f) = p.TryGetNextFrame() {
                            // Full channel: drop this frame; the engine drains to the newest anyway.
                            let _ = tx.try_send(f);
                        }
                    }
                }
                Ok(())
            });
        pool.FrameArrived(&handler).context("FrameArrived")?;
        *pool_cell.lock().map_err(|_| anyhow::anyhow!("pool lock"))? = Some(pool.clone());

        let session = pool
            .CreateCaptureSession(&item)
            .context("CreateCaptureSession")?;
        // The real (hardware) cursor stays visible above the mirror; capturing
        // it too would draw a second, laggy cursor. Both setters need newer
        // Windows builds, so failures are ignored.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().context("StartCapture")?;

        let mirror_hwnd = create_mirror_window(size.Width, size.Height)?;
        let swap_size = (size.Width.max(1) as u32, size.Height.max(1) as u32);
        let (swap, swap_rtv) = create_swapchain(&device, mirror_hwnd, swap_size.0, swap_size.1)?;
        let shaders = compile_shaders(&device, include_str!("../shader.hlsl"))?;

        let det = Target::new(&device, DETECTION_W, DETECTION_H, DXGI_FORMAT_R32G32B32A32_FLOAT, false)?;
        let det_staging = create_texture(
            &device,
            DETECTION_W,
            DETECTION_H,
            DXGI_FORMAT_R32G32B32A32_FLOAT,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        let scale_tex = create_texture(
            &device,
            TILE_COLS as u32,
            TILE_ROWS as u32,
            DXGI_FORMAT_R32G32_FLOAT,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
        )?;
        let scale_srv = create_srv(&device, &scale_tex)?;

        let mut cb = None;
        unsafe {
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: std::mem::size_of::<ShaderParams>() as u32,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut cb),
            )?;
        }
        let cb = cb.context("constant buffer")?;

        let mut samp = None;
        unsafe {
            device
                .CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    },
                    Some(&mut samp),
                )?;
        }
        let samp = samp.context("sampler")?;

        let mut rs = None;
        unsafe {
            device
                .CreateRasterizerState(
                    &D3D11_RASTERIZER_DESC {
                        FillMode: windows::Win32::Graphics::Direct3D11::D3D11_FILL_SOLID,
                        CullMode: D3D11_CULL_NONE,
                        ..Default::default()
                    },
                    Some(&mut rs),
                )?;
        }
        let rs = rs.context("rs")?;

        let filter = TileFilter::new(config.filter);
        Ok(Self {
            target_hwnd,
            mirror_hwnd,
            device,
            ctx,
            swap,
            swap_rtv: Some(swap_rtv),
            swap_size,
            shaders,
            samp,
            rs,
            cb,
            det,
            det_staging,
            scale_tex,
            scale_srv,
            last_client_rect: None,
            _pool_cell: pool_cell,
            pool,
            pool_size: size,
            winrt_device,
            _session: session,
            frame_rx,
            config,
            filter,
            stats: vec![TileStats::default(); TILE_COLS * TILE_ROWS],
            scales: vec![1.0; TILE_COLS * TILE_ROWS * 2],
            surfaces: None,
            last_capture_time: None,
            last_present: Instant::now(),
        })
    }

    fn set_config(&mut self, config: FlashSafeConfig) {
        self.filter.set_params(config.filter);
        self.config = config;
    }

    /// Wait briefly for a frame and return the newest one available.
    fn newest_frame(&self) -> Option<Direct3D11CaptureFrame> {
        let mut frame = self.frame_rx.recv_timeout(Duration::from_millis(8)).ok()?;
        while let Ok(f) = self.frame_rx.try_recv() {
            frame = f;
        }
        Some(frame)
    }

    fn texture_from_frame(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D> {
        let surface = frame.Surface().context("Surface")?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().context("cast access")?;
        unsafe { access.GetInterface::<ID3D11Texture2D>() }.context("GetInterface")
    }

    /// Returns whether a frame was presented.
    fn tick(&mut self) -> Result<bool> {
        let dt = if let Some(frame) = self.newest_frame() {
            let content = frame.ContentSize().context("ContentSize")?;
            let copied = {
                let tex = Self::texture_from_frame(&frame)?;
                self.copy_client_area(&tex, content.Width.max(0) as u32, content.Height.max(0) as u32)?
            };
            let stamp = frame.SystemRelativeTime().ok().map(|t| t.Duration);
            // Hand the WGC buffer back to the pool as soon as it's copied.
            let _ = frame.Close();
            if content.Width > 0 && content.Height > 0 && content != self.pool_size {
                // The window was resized: capture at the new size from now on.
                self.pool
                    .Recreate(&self.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)
                    .context("frame pool Recreate")?;
                self.pool_size = content;
            }
            if !copied {
                return Ok(false);
            }
            let dt = match (stamp, self.last_capture_time) {
                (Some(now), Some(prev)) if now > prev => (now - prev) as f32 / 1e7,
                _ => self.last_present.elapsed().as_secs_f32(),
            };
            self.last_capture_time = stamp;
            dt
        } else if self.surfaces.is_some()
            && !self.filter.is_settled()
            && self.last_present.elapsed() >= REPRESENT_INTERVAL
        {
            // Game drew nothing new but the display is still catching up: re-present.
            self.last_present.elapsed().as_secs_f32()
        } else {
            return Ok(false);
        };
        self.process(dt)?;
        Ok(true)
    }

    /// Copy the target's client area out of the captured texture. Returns
    /// false when there is nothing to show (minimized, zero-size client).
    fn copy_client_area(&mut self, src: &ID3D11Texture2D, content_w: u32, content_h: u32) -> Result<bool> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        let (cw, ch) = (content_w.min(desc.Width), content_h.min(desc.Height));
        if cw == 0 || ch == 0 {
            return Ok(false);
        }
        let Some(area) = client_area(self.target_hwnd, cw, ch) else {
            return Ok(false);
        };
        let stale = self
            .surfaces
            .as_ref()
            .is_none_or(|s| s.width != area.width || s.height != area.height);
        if stale {
            self.surfaces = Some(Surfaces::new(&self.device, area.width, area.height)?);
        }
        let surf = self.surfaces.as_ref().context("surfaces")?;
        let bx = D3D11_BOX {
            left: area.left,
            top: area.top,
            front: 0,
            right: area.left + area.width,
            bottom: area.top + area.height,
            back: 1,
        };
        unsafe { self.ctx.CopySubresourceRegion(&surf.frame, 0, 0, 0, 0, src, 0, Some(&bx)) };
        sync_mirror_if_moved(self.mirror_hwnd, area.screen, &mut self.last_client_rect);
        Ok(true)
    }

    fn process(&mut self, dt: f32) -> Result<()> {
        let Some(mut surf) = self.surfaces.take() else {
            return Ok(());
        };
        let result = self.run_passes(&mut surf, dt);
        self.surfaces = Some(surf);
        result
    }

    fn run_passes(&mut self, surf: &mut Surfaces, dt: f32) -> Result<()> {
        let (w, h) = (surf.width, surf.height);
        let params = ShaderParams {
            min_gain: self.filter.params().min_gain,
            stats_level: surf.stats_level,
            _pad: [0.0; 2],
        };
        unsafe {
            self.ctx.UpdateSubresource(&self.cb, 0, None, (&params as *const ShaderParams).cast(), 0, 0);
        }
        let frame = Some(surf.frame_srv.clone());

        if !surf.primed {
            // Start the display history at the current frame so nothing ramps at startup.
            self.draw(&[Some(surf.hist[surf.cur].rtv.clone())], std::slice::from_ref(&frame), &self.shaders.prime.clone(), w, h);
            surf.primed = true;
        }
        let hist = Some(surf.hist[surf.cur].srv.clone());

        // Per-pixel stats → mip chain → small area-averaged image → CPU tiles.
        self.draw(&[Some(surf.stats.rtv.clone())], &[frame.clone(), hist.clone()], &self.shaders.stats.clone(), w, h);
        unsafe { self.ctx.GenerateMips(&surf.stats.srv) };
        self.draw(
            &[Some(self.det.rtv.clone())],
            &[Some(surf.stats.srv.clone())],
            &self.shaders.downsample.clone(),
            DETECTION_W,
            DETECTION_H,
        );
        unsafe {
            self.ctx.CopyResource(&self.det_staging, &self.det.tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.det_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let stride = mapped.RowPitch as usize / std::mem::size_of::<f32>();
            let len = stride * DETECTION_H as usize;
            let data = std::slice::from_raw_parts(mapped.pData as *const f32, len);
            tile_stats_f32x4(
                data,
                stride,
                DETECTION_W as usize,
                DETECTION_H as usize,
                TILE_COLS,
                TILE_ROWS,
                &mut self.stats,
            );
            self.ctx.Unmap(&self.det_staging, 0);
        }

        self.filter.update(&self.stats, dt);
        if self.config.enabled {
            let (rise, fall) = (self.filter.rise_scales(), self.filter.fall_scales());
            for (i, pair) in self.scales.chunks_exact_mut(2).enumerate() {
                pair[0] = rise[i];
                pair[1] = fall[i];
            }
        } else {
            self.scales.fill(1.0);
        }
        unsafe {
            self.ctx.UpdateSubresource(
                &self.scale_tex,
                0,
                None,
                self.scales.as_ptr().cast(),
                (TILE_COLS * 2 * std::mem::size_of::<f32>()) as u32,
                0,
            );
        }

        // Apply: filtered colour to the mirror, and the same colour into the
        // next history texture.
        if self.swap_size != (w, h) {
            self.resize_swapchain(w, h)?;
        }
        let swap_rtv = self.swap_rtv.clone().context("swapchain RTV")?;
        let next = 1 - surf.cur;
        self.draw(
            &[Some(swap_rtv), Some(surf.hist[next].rtv.clone())],
            &[frame, hist, Some(self.scale_srv.clone())],
            &self.shaders.apply.clone(),
            w,
            h,
        );
        surf.cur = next;
        unsafe {
            // Interval 0 = do not block on vsync (mirror only; lowers latency vs blocking Present).
            let _ = self.swap.Present(0, DXGI_PRESENT(0));
        }
        self.last_present = Instant::now();
        Ok(())
    }

    fn resize_swapchain(&mut self, w: u32, h: u32) -> Result<()> {
        unsafe {
            self.ctx.OMSetRenderTargets(Some(&[]), None);
            // Every reference to the back buffer must be released before ResizeBuffers.
            self.swap_rtv = None;
            self.ctx.Flush();
            self.swap
                .ResizeBuffers(0, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, Default::default())
                .context("ResizeBuffers")?;
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            let mut rtv = None;
            self.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            self.swap_rtv = Some(rtv.context("rtv")?);
        }
        self.swap_size = (w, h);
        self.last_client_rect = None;
        Ok(())
    }

    /// Full-screen triangle into `rtvs` reading `srvs` (t0, t1, ...).
    fn draw(
        &self,
        rtvs: &[Option<ID3D11RenderTargetView>],
        srvs: &[Option<ID3D11ShaderResourceView>],
        ps: &ID3D11PixelShader,
        w: u32,
        h: u32,
    ) {
        let vp = D3D11_VIEWPORT {
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
            TopLeftX: 0.0,
            TopLeftY: 0.0,
        };
        let unbind: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];
        unsafe {
            self.ctx.RSSetViewports(Some(&[vp]));
            self.ctx.OMSetRenderTargets(Some(rtvs), None);
            self.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.shaders.vs, None);
            self.ctx.PSSetShader(ps, None);
            self.ctx.PSSetShaderResources(0, Some(srvs));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.samp.clone())]));
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.ctx.RSSetState(&self.rs);
            self.ctx.OMSetBlendState(None, None, u32::MAX);
            self.ctx.OMSetDepthStencilState(None, 0);
            self.ctx.Draw(3, 0);
            // Unbind so the next pass can use these textures the other way round.
            self.ctx.OMSetRenderTargets(Some(&[]), None);
            self.ctx.PSSetShaderResources(0, Some(&unbind));
        }
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.mirror_hwnd);
        }
    }
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
    }
    tex.context("CreateTexture2D")
}

fn create_srv(device: &ID3D11Device, tex: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
    let mut srv = None;
    unsafe {
        device.CreateShaderResourceView(tex, None, Some(&mut srv))?;
    }
    srv.context("CreateShaderResourceView")
}
