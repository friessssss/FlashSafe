//! WGC → D3D11 texture → tile luminance → [`TileFilter`] gains → gain pixel shader → mirror `HWND`.
//!
//! Interim engine: the filter runs on the CPU from a small readback, and the
//! mirror still uses an HWND swapchain. See `docs/mvp-plan.md` (Phases 2–3)
//! for the click-through DirectComposition overlay and GPU filter that
//! replace this.

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flashsafe_core::{
    tile_luminance_bgra, BgraFrame, FlashSafeConfig, TileFilter, TILE_COLS, TILE_ROWS,
};
use parking_lot::RwLock;
use serde::Serialize;
use windows::core::{BOOL, IInspectable, Interface, s};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::UI::WindowId;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CULL_NONE, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_READ,
    D3D11_RASTERIZER_DESC, D3D11_SAMPLER_DESC, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11CreateDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32_FLOAT, DXGI_SAMPLE_DESC};
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GetWindowRect,
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

fn compile_shaders(
    device: &ID3D11Device,
    src: &str,
) -> Result<(ID3D11VertexShader, ID3D11PixelShader, ID3D11PixelShader)> {
    let mut errors = None;
    let mut vs_blob = None;
    unsafe {
        D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            None,
            None,
            None,
            s!("VSMain"),
            s!("vs_5_0"),
            0,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            &mut vs_blob,
            Some(&mut errors),
        )?;
    }
    let vs_blob = vs_blob.context("vs_blob")?;
    let mut ps_blob = None;
    unsafe {
        D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            None,
            None,
            None,
            s!("PSMain"),
            s!("ps_5_0"),
            0,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            &mut ps_blob,
            Some(&mut errors),
        )?;
    }
    let ps_blob = ps_blob.context("ps_blob")?;
    let mut ps_blit_blob = None;
    unsafe {
        D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            None,
            None,
            None,
            s!("PSBlit"),
            s!("ps_5_0"),
            0,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            &mut ps_blit_blob,
            Some(&mut errors),
        )?;
    }
    let ps_blit_blob = ps_blit_blob.context("ps_blit_blob")?;
    let vs_data = unsafe {
        std::slice::from_raw_parts(
            vs_blob.GetBufferPointer() as *const u8,
            vs_blob.GetBufferSize(),
        )
    };
    let ps_data = unsafe {
        std::slice::from_raw_parts(
            ps_blob.GetBufferPointer() as *const u8,
            ps_blob.GetBufferSize(),
        )
    };
    let ps_blit_data = unsafe {
        std::slice::from_raw_parts(
            ps_blit_blob.GetBufferPointer() as *const u8,
            ps_blit_blob.GetBufferSize(),
        )
    };
    let mut vs = None;
    let mut ps = None;
    let mut ps_blit = None;
    unsafe {
        device.CreateVertexShader(vs_data, None, Some(&mut vs))?;
        device.CreatePixelShader(ps_data, None, Some(&mut ps))?;
        device.CreatePixelShader(ps_blit_data, None, Some(&mut ps_blit))?;
    }
    Ok((
        vs.context("vs")?,
        ps.context("ps")?,
        ps_blit.context("ps_blit")?,
    ))
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

/// Only `SetWindowPos` when the target moves/resizes — avoids redundant DWM work every frame.
fn sync_mirror_if_moved(target: HWND, mirror: HWND, w: u32, h: u32, last: &mut Option<RECT>) {
    unsafe {
        if !IsWindow(Some(target)).as_bool() {
            return;
        }
        let mut r = RECT::default();
        if GetWindowRect(target, &mut r).is_err() {
            return;
        }
        if last.as_ref() == Some(&r) {
            return;
        }
        *last = Some(r);
        let _ = SetWindowPos(
            mirror,
            Some(HWND_TOPMOST),
            r.left,
            r.top,
            w as i32,
            h as i32,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// GPU stats path resolution (≈57k pixels read on CPU vs full 4K framebuffer).
const DETECTION_W: u32 = 320;
const DETECTION_H: u32 = 180;
/// While the filter is still ramping, re-present the last frame at this
/// interval even if the game hasn't drawn a new one.
const REPRESENT_INTERVAL: Duration = Duration::from_millis(16);

/// Our own copy of the latest captured frame, so the WGC buffer can go back
/// to the pool immediately and we can re-present while the filter settles.
struct FrameCopy {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

struct ActiveSession {
    target_hwnd: HWND,
    mirror_hwnd: HWND,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    swap_rtv: Option<ID3D11RenderTargetView>,
    swap_size: (u32, u32),
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    ps_blit: ID3D11PixelShader,
    samp: ID3D11SamplerState,
    rs: ID3D11RasterizerState,
    det_rtv: ID3D11RenderTargetView,
    det_tex: ID3D11Texture2D,
    det_staging: ID3D11Texture2D,
    gain_tex: ID3D11Texture2D,
    gain_srv: ID3D11ShaderResourceView,
    last_target_rect: Option<RECT>,
    _pool_cell: Arc<Mutex<Option<Direct3D11CaptureFramePool>>>,
    _pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    frame_rx: Receiver<Direct3D11CaptureFrame>,
    config: FlashSafeConfig,
    filter: TileFilter,
    tiles: Vec<f32>,
    frame_copy: Option<FrameCopy>,
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
        let winrt = create_winrt_device(&device)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt,
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
        let hlsl = include_str!("../shader.hlsl");
        let (vs, ps, ps_blit) = compile_shaders(&device, hlsl)?;

        let det_tex = create_texture(
            &device,
            DETECTION_W,
            DETECTION_H,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            0,
        )?;
        let mut det_rtv = None;
        unsafe {
            device.CreateRenderTargetView(&det_tex, None, Some(&mut det_rtv))?;
        }
        let det_rtv = det_rtv.context("det_rtv")?;
        let det_staging = create_texture(
            &device,
            DETECTION_W,
            DETECTION_H,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;

        let gain_tex = create_texture(
            &device,
            TILE_COLS as u32,
            TILE_ROWS as u32,
            DXGI_FORMAT_R32_FLOAT,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
        )?;
        let gain_srv = create_srv(&device, &gain_tex)?;

        let mut samp = None;
        unsafe {
            device
                .CreateSamplerState(
                    &D3D11_SAMPLER_DESC {
                        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                        AddressU: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
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
        let sess = Self {
            target_hwnd,
            mirror_hwnd,
            device,
            ctx,
            swap,
            swap_rtv: Some(swap_rtv),
            swap_size,
            vs,
            ps,
            ps_blit,
            samp,
            rs,
            det_rtv,
            det_tex,
            det_staging,
            gain_tex,
            gain_srv,
            last_target_rect: None,
            _pool_cell: pool_cell,
            _pool: pool,
            _session: session,
            frame_rx,
            config,
            filter,
            tiles: vec![0.0; TILE_COLS * TILE_ROWS],
            frame_copy: None,
            last_capture_time: None,
            last_present: Instant::now(),
        };
        sess.upload_gains(&vec![1.0; TILE_COLS * TILE_ROWS]);
        Ok(sess)
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
            let tex = Self::texture_from_frame(&frame)?;
            self.copy_frame(&tex)?;
            let stamp = frame.SystemRelativeTime().ok().map(|t| t.Duration);
            // Hand the WGC buffer back to the pool as soon as it's copied.
            drop(tex);
            let _ = frame.Close();
            let dt = match (stamp, self.last_capture_time) {
                (Some(now), Some(prev)) if now > prev => (now - prev) as f32 / 1e7,
                _ => self.last_present.elapsed().as_secs_f32(),
            };
            self.last_capture_time = stamp;
            dt
        } else if self.frame_copy.is_some()
            && !self.filter.is_settled()
            && self.last_present.elapsed() >= REPRESENT_INTERVAL
        {
            // Game drew nothing new but the filter is still ramping: re-present.
            self.last_present.elapsed().as_secs_f32()
        } else {
            return Ok(false);
        };
        self.process(dt)?;
        Ok(true)
    }

    fn copy_frame(&mut self, src: &ID3D11Texture2D) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        let stale = self
            .frame_copy
            .as_ref()
            .is_none_or(|c| c.width != desc.Width || c.height != desc.Height);
        if stale {
            let tex = create_texture(
                &self.device,
                desc.Width,
                desc.Height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_DEFAULT,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
                0,
            )?;
            let srv = create_srv(&self.device, &tex)?;
            self.frame_copy = Some(FrameCopy {
                tex,
                srv,
                width: desc.Width,
                height: desc.Height,
            });
        }
        let copy = self.frame_copy.as_ref().context("frame copy")?;
        unsafe { self.ctx.CopyResource(&copy.tex, src) };
        Ok(())
    }

    fn process(&mut self, dt: f32) -> Result<()> {
        let (srv, w, h) = {
            let c = self.frame_copy.as_ref().context("frame copy")?;
            (c.srv.clone(), c.width, c.height)
        };

        // Downscale → readback → tile luminance → filter.
        self.draw_fullscreen(&self.det_rtv.clone(), &srv, &self.ps_blit.clone(), DETECTION_W, DETECTION_H, false);
        unsafe {
            self.ctx.CopyResource(&self.det_staging, &self.det_tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.det_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row_pitch = mapped.RowPitch as usize;
            let len = row_pitch * DETECTION_H as usize;
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, len);
            let frame = BgraFrame {
                data,
                row_pitch,
                width: DETECTION_W as usize,
                height: DETECTION_H as usize,
            };
            tile_luminance_bgra(frame, TILE_COLS, TILE_ROWS, 1, &mut self.tiles);
            self.ctx.Unmap(&self.det_staging, 0);
        }
        let gains = self.filter.update(&self.tiles, dt).to_vec();
        if self.config.enabled {
            self.upload_gains(&gains);
        } else {
            self.upload_gains(&vec![1.0; gains.len()]);
        }

        sync_mirror_if_moved(self.target_hwnd, self.mirror_hwnd, w, h, &mut self.last_target_rect);
        if self.swap_size != (w, h) {
            self.resize_swapchain(w, h)?;
        }
        let rtv = self.swap_rtv.clone().context("swapchain RTV")?;
        self.draw_fullscreen(&rtv, &srv, &self.ps.clone(), w, h, true);
        unsafe {
            // Interval 0 = do not block on vsync (mirror only; lowers latency vs blocking Present).
            let _ = self.swap.Present(0, DXGI_PRESENT(0));
        }
        self.last_present = Instant::now();
        Ok(())
    }

    fn upload_gains(&self, gains: &[f32]) {
        unsafe {
            self.ctx.UpdateSubresource(
                &self.gain_tex,
                0,
                None,
                gains.as_ptr().cast(),
                (TILE_COLS * std::mem::size_of::<f32>()) as u32,
                0,
            );
        }
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
        self.last_target_rect = None;
        Ok(())
    }

    fn draw_fullscreen(
        &self,
        rtv: &ID3D11RenderTargetView,
        src: &ID3D11ShaderResourceView,
        ps: &ID3D11PixelShader,
        w: u32,
        h: u32,
        with_gains: bool,
    ) {
        let vp = D3D11_VIEWPORT {
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
            TopLeftX: 0.0,
            TopLeftY: 0.0,
        };
        let gains = with_gains.then(|| self.gain_srv.clone());
        unsafe {
            self.ctx.RSSetViewports(Some(&[vp]));
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(ps, None);
            self.ctx.PSSetShaderResources(0, Some(&[Some(src.clone()), gains]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.samp.clone())]));
            self.ctx.RSSetState(&self.rs);
            self.ctx.OMSetBlendState(None, None, u32::MAX);
            self.ctx.OMSetDepthStencilState(None, 0);
            self.ctx.Draw(3, 0);
            self.ctx.OMSetRenderTargets(Some(&[]), None);
            self.ctx.PSSetShaderResources(0, Some(&[None, None]));
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
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
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
