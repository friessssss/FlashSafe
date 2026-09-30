//! WGC → D3D11 texture → CPU downsample stats → mitigation pixel shader → mirror `HWND`.

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flashsafe_core::{
    mitigation::smooth_toward, DownsampleStats, FastFlashDetector, FastFlashDetectorConfig,
    FlashMetrics, FlashSafeConfig,
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
    D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CULL_NONE, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_READ,
    D3D11_MAP_WRITE_DISCARD, D3D11_RASTERIZER_DESC, D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11Resource,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11CreateDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
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
    pub flashes: u64,
    pub mitigation: f32,
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
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(EngineCommand::Stop) => {
                session = None;
                let mut s = stats.write();
                s.running = false;
                s.mitigation = 0.0;
            }
            Ok(EngineCommand::UpdateConfig(c)) => {
                if let Some(ref mut s) = session {
                    s.config = c;
                }
            }
            Ok(EngineCommand::Start { hwnd, config }) => {
                session = None;
                let mut s = stats.write();
                s.last_error = None;
                match ActiveSession::new(HWND(hwnd as *mut c_void), config) {
                    Ok(sess) => {
                        session = Some(sess);
                        s.running = true;
                    }
                    Err(e) => {
                        s.running = false;
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
                Ok(()) => {
                    let mut s = stats.write();
                    s.frames = s.frames.saturating_add(1);
                    s.mitigation = sess.smoothed_mitigation;
                    s.flashes = sess.flash_accum;
                    fps_frames += 1;
                    if fps_tick.elapsed() >= Duration::from_millis(500) {
                        s.fps = fps_frames as f32 / fps_tick.elapsed().as_secs_f32().max(0.001);
                        fps_frames = 0;
                        fps_tick = Instant::now();
                    }
                }
                Err(e) => {
                    stats.write().last_error = Some(format!("{e:#}"));
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

struct ActiveSession {
    target_hwnd: HWND,
    mirror_hwnd: HWND,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    ps_blit: ID3D11PixelShader,
    samp: ID3D11SamplerState,
    rs: ID3D11RasterizerState,
    cb: ID3D11Buffer,
    det_tex: ID3D11Texture2D,
    det_rtv: ID3D11RenderTargetView,
    det_staging: ID3D11Texture2D,
    last_target_rect: Option<RECT>,
    _pool_cell: Arc<Mutex<Option<Direct3D11CaptureFramePool>>>,
    _pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    frame_rx: Receiver<Direct3D11CaptureFrame>,
    config: FlashSafeConfig,
    detector: FastFlashDetector,
    smoothed_mitigation: f32,
    last_frame_time: Option<Instant>,
    flash_accum: u64,
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
        let (tx, frame_rx) = mpsc::sync_channel(8);
        let cell = Arc::clone(&pool_cell);
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_pool, _args| {
                if let Ok(g) = cell.lock() {
                    if let Some(ref p) = *g {
                        if let Ok(f) = p.TryGetNextFrame() {
                            let _ = tx.try_send(f);
                        }
                    }
                }
                Ok(())
            });
        pool.FrameArrived(&handler).context("FrameArrived")?;
        *pool_cell.lock().unwrap() = Some(pool.clone());

        let session = pool
            .CreateCaptureSession(&item)
            .context("CreateCaptureSession")?;
        session.StartCapture().context("StartCapture")?;

        let mirror_hwnd = create_mirror_window(size.Width, size.Height)?;
        let (swap, _rtv) = create_swapchain(&device, mirror_hwnd, size.Width as u32, size.Height as u32)?;
        let hlsl = include_str!("../shader.hlsl");
        let (vs, ps, ps_blit) = compile_shaders(&device, hlsl)?;

        let det_bind = (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32;
        let det_desc = D3D11_TEXTURE2D_DESC {
            Width: DETECTION_W,
            Height: DETECTION_H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: det_bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut det_tex = None;
        unsafe {
            device.CreateTexture2D(&det_desc, None, Some(&mut det_tex))?;
        }
        let det_tex = det_tex.context("det_tex")?;
        let mut det_rtv = None;
        unsafe {
            device.CreateRenderTargetView(&det_tex, None, Some(&mut det_rtv))?;
        }
        let det_rtv = det_rtv.context("det_rtv")?;

        let det_staging_desc = D3D11_TEXTURE2D_DESC {
            Width: DETECTION_W,
            Height: DETECTION_H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut det_staging = None;
        unsafe {
            device.CreateTexture2D(&det_staging_desc, None, Some(&mut det_staging))?;
        }
        let det_staging = det_staging.context("det_staging")?;

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

        let cb_init = [0u8; 32];
        let mut cb = None;
        unsafe {
            device
                .CreateBuffer(
                    &windows::Win32::Graphics::Direct3D11::D3D11_BUFFER_DESC {
                        ByteWidth: 32,
                        Usage: D3D11_USAGE_DYNAMIC,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_WRITE.0 as u32,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    Some(&D3D11_SUBRESOURCE_DATA {
                        pSysMem: cb_init.as_ptr().cast(),
                        SysMemPitch: 0,
                        SysMemSlicePitch: 0,
                    }),
                    Some(&mut cb),
                )?;
        }
        let cb = cb.context("cb")?;

        let detector = FastFlashDetector::new(FastFlashDetectorConfig {
            spike_delta_threshold: config.pipeline.spike_delta_threshold,
            peak_clip_cell_fraction: config.pipeline.peak_clip_cell_fraction,
            pattern_sensitivity: config.pipeline.pattern_sensitivity,
        });

        Ok(Self {
            target_hwnd,
            mirror_hwnd,
            device,
            ctx,
            swap,
            vs,
            ps,
            ps_blit,
            samp,
            rs,
            cb,
            det_tex,
            det_rtv,
            det_staging,
            last_target_rect: None,
            _pool_cell: pool_cell,
            _pool: pool,
            _session: session,
            frame_rx,
            config,
            detector,
            smoothed_mitigation: 0.0,
            last_frame_time: None,
            flash_accum: 0,
        })
    }

    fn blit_to_detection(&mut self, src_tex: &ID3D11Texture2D) -> Result<()> {
        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv = None;
        unsafe {
            self.device
                .CreateShaderResourceView(src_tex, Some(&srv_desc), Some(&mut srv))?;
        }
        let srv = srv.context("blit srv")?;
        let vp = D3D11_VIEWPORT {
            Width: DETECTION_W as f32,
            Height: DETECTION_H as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
            TopLeftX: 0.0,
            TopLeftY: 0.0,
        };
        let null_srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.ctx.RSSetViewports(Some(&[vp]));
            self.ctx.OMSetRenderTargets(Some(&[Some(self.det_rtv.clone())]), None);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps_blit, None);
            self.ctx
                .PSSetShaderResources(0, Some(&[Some(srv.clone()), null_srv.clone()]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.samp.clone())]));
            self.ctx.RSSetState(&self.rs);
            self.ctx.OMSetBlendState(None, None, u32::MAX);
            self.ctx.OMSetDepthStencilState(None, 0);
            self.ctx.Draw(3, 0);
            self.ctx.OMSetRenderTargets(Some(&[]), None);
            self.ctx
                .PSSetShaderResources(0, Some(&[null_srv.clone(), null_srv]));
        }
        Ok(())
    }

    fn texture_from_frame(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D> {
        let surface = frame.Surface().context("Surface")?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().context("cast access")?;
        unsafe { access.GetInterface::<ID3D11Texture2D>() }.context("GetInterface")
    }

    fn tick(&mut self) -> Result<()> {
        let frame = match self.frame_rx.recv_timeout(Duration::from_millis(12)) {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };

        let tex = Self::texture_from_frame(&frame)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let w = desc.Width;
        let h = desc.Height;

        self.blit_to_detection(&tex)?;
        let stats = unsafe {
            let src_res: ID3D11Resource = self.det_tex.cast().unwrap();
            let dst_res: ID3D11Resource = self.det_staging.cast().unwrap();
            self.ctx.CopyResource(&dst_res, &src_res);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.det_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row_pitch = mapped.RowPitch as usize;
            let nbytes = row_pitch * DETECTION_H as usize;
            let slice = std::slice::from_raw_parts(mapped.pData as *const u8, nbytes);
            let grid = self.config.pipeline.grid_size;
            let s = DownsampleStats::from_bgra_strided(slice, row_pitch, DETECTION_W, DETECTION_H, grid);
            self.ctx.Unmap(&self.det_staging, 0);
            s
        };

        if self.config.present_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.config.present_delay_ms as u64));
        }

        sync_mirror_if_moved(
            self.target_hwnd,
            self.mirror_hwnd,
            w,
            h,
            &mut self.last_target_rect,
        );

        let now = Instant::now();
        self.detector.update_config(FastFlashDetectorConfig {
            spike_delta_threshold: self.config.pipeline.spike_delta_threshold,
            peak_clip_cell_fraction: self.config.pipeline.peak_clip_cell_fraction,
            pattern_sensitivity: self.config.pipeline.pattern_sensitivity,
        });
        let m: FlashMetrics = self.detector.push(&stats, now);
        if m.raw_threat > 0.2 {
            self.flash_accum = self.flash_accum.saturating_add(1);
        }

        let target = if self.config.enabled {
            (m.raw_threat * self.config.pipeline.max_mitigation).min(1.0)
        } else {
            0.0
        };
        let dt = self
            .last_frame_time
            .map(|t| now.duration_since(t).as_secs_f32())
            .unwrap_or(0.016);
        self.last_frame_time = Some(now);
        self.smoothed_mitigation = smooth_toward(
            self.smoothed_mitigation,
            target,
            dt,
            self.config.pipeline.attack_ms / 1000.0,
            self.config.pipeline.release_ms / 1000.0,
        );

        self.present(&tex, w, h)?;
        Ok(())
    }

    fn present(&mut self, src_tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<()> {
        let p = &self.config.pipeline;
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        let mut rtv = None;
        unsafe {
            self.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        }
        let rtv = rtv.context("rtv")?;

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv0 = None;
        let mut srv1 = None;
        unsafe {
            self.device
                .CreateShaderResourceView(src_tex, Some(&srv_desc), Some(&mut srv0))?;
            self.device
                .CreateShaderResourceView(src_tex, Some(&srv_desc), Some(&mut srv1))?;
        }
        let srv0 = srv0.context("srv0")?;
        let srv1 = srv1.context("srv1")?;

        let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx
                .Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .ok()
                .context("Map CB")?;
            let dst = mapped.pData as *mut f32;
            *dst.add(0) = self.smoothed_mitigation;
            *dst.add(1) = p.exposure_scale;
            *dst.add(2) = p.highlight_knee;
            *dst.add(3) = p.temporal_blend;
            *dst.add(4) = p.desaturate_on_threat;
            self.ctx.Unmap(&self.cb, 0);
        }

        let vp = D3D11_VIEWPORT {
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
            TopLeftX: 0.0,
            TopLeftY: 0.0,
        };
        unsafe {
            self.ctx.RSSetViewports(Some(&[vp]));
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.ctx.PSSetShaderResources(0, Some(&[Some(srv0), Some(srv1)]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.samp.clone())]));
            self.ctx.RSSetState(&self.rs);
            self.ctx.OMSetBlendState(None, None, u32::MAX);
            self.ctx.OMSetDepthStencilState(None, 0);
            self.ctx.Draw(3, 0);
            // Interval 0 = do not block on vsync (mirror only; lowers latency vs blocking Present).
            let _ = self.swap.Present(0, DXGI_PRESENT(0));
        }
        Ok(())
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.mirror_hwnd);
        }
    }
}
