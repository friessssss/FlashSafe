//! Screen-capture module backed by the DXGI Desktop Duplication API (Windows).
//!
//! Provides a [`Capturer`] that continuously yields raw BGRA8 frames from the
//! desktop compositor with sub-millisecond grab latency.  On non-Windows
//! targets the type stubs out with an informative error so the rest of the
//! crate still compiles.
//!
//! # Topology changes
//! When a monitor is plugged or unplugged Windows signals `DXGI_ERROR_ACCESS_LOST`
//! on the next `AcquireNextFrame` call.  [`Capturer::grab`] handles this
//! transparently by tearing down the old duplication interface and rebuilding
//! it before returning the next frame.

// ---------------------------------------------------------------------------
// Public types (platform-independent)
// ---------------------------------------------------------------------------

use anyhow::Result;

/// A single captured frame in BGRA8 (B, G, R, A — 1 byte each) format.
#[derive(Clone)]
pub struct RawFrame {
    /// Width of the frame in pixels.
    pub width: u32,
    /// Height of the frame in pixels.
    pub height: u32,
    /// Raw pixel bytes in BGRA8 order; length = `width * height * 4`.
    pub data: Vec<u8>,
}

/// Configuration for the DXGI screen capturer.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// DXGI adapter to use (0 = primary GPU).
    pub adapter_index: u32,
    /// Output (monitor) index on that adapter (0 = primary monitor).
    pub output_index: u32,
    /// How long [`Capturer::grab`] waits for a new frame before returning
    /// `Ok(None)`.  Increase this if you are happy to block; decrease if you
    /// want a polling loop.
    pub acquire_timeout_ms: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            adapter_index: 0,
            output_index: 0,
            acquire_timeout_ms: 100,
        }
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{CaptureConfig, RawFrame, Result};
    use anyhow::Context;
    use tracing::{debug, warn};
    use windows::core::Interface;
    use windows::{
        Win32::Foundation::HMODULE,
        Win32::Graphics::Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_11_0,
        },
        Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext,
            ID3D11Resource, ID3D11Texture2D,
        },
        Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
            IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
        },
        Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
    };

    pub struct Capturer {
        config: CaptureConfig,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        /// Lazily created; recreated whenever resolution changes.
        staging: Option<ID3D11Texture2D>,
        width: u32,
        height: u32,
    }

    impl Capturer {
        pub fn new(config: CaptureConfig) -> Result<Self> {
            let (device, context, duplication, width, height) =
                init_dxgi(config.adapter_index, config.output_index)?;
            debug!(width, height, "DXGI capturer initialised");
            Ok(Self {
                config,
                device,
                context,
                duplication,
                staging: None,
                width,
                height,
            })
        }

        pub fn width(&self) -> u32 {
            self.width
        }
        pub fn height(&self) -> u32 {
            self.height
        }

        /// Grab the next frame from the compositor.
        ///
        /// Returns `Ok(None)` when no new frame arrived within the configured
        /// timeout — the caller should simply call `grab` again.
        pub fn grab(&mut self) -> Result<Option<RawFrame>> {
            loop {
                unsafe {
                    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                    let mut resource: Option<IDXGIResource> = None;

                    let result = self.duplication.AcquireNextFrame(
                        self.config.acquire_timeout_ms,
                        &mut frame_info,
                        &mut resource,
                    );

                    match result {
                        Ok(()) => {}
                        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                            return Ok(None)
                        }
                        Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                            warn!(
                                "DXGI_ERROR_ACCESS_LOST — display topology \
                                 changed, recreating output duplication"
                            );
                            let (dup, w, h) = recreate_duplication(
                                &self.device,
                                self.config.adapter_index,
                                self.config.output_index,
                            )?;
                            self.duplication = dup;
                            self.width = w;
                            self.height = h;
                            self.staging = None; // size may have changed
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }

                    let resource =
                        resource.context("AcquireNextFrame returned null resource")?;

                    let data = self.copy_to_cpu(&resource)?;
                    self.duplication.ReleaseFrame()?;

                    return Ok(Some(RawFrame {
                        width: self.width,
                        height: self.height,
                        data,
                    }));
                }
            }
        }

        /// Copy the GPU texture to CPU-accessible memory and return the BGRA8 bytes.
        fn copy_to_cpu(&mut self, resource: &IDXGIResource) -> Result<Vec<u8>> {
            unsafe {
                let texture: ID3D11Texture2D = resource.cast()?;

                // Lazily allocate (or reallocate after resolution change).
                if self.staging.is_none() {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: self.width,
                        Height: self.height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        // BindFlags / CPUAccessFlags / MiscFlags are u32 in
                        // windows-rs 0.58 (not the typed flag wrappers).
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut staging = None;
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut staging))
                        .context("CreateTexture2D (staging) failed")?;
                    self.staging =
                        Some(staging.context("CreateTexture2D returned null")?);
                }

                let staging = self.staging.as_ref().unwrap();

                // GPU-side blit: desktop texture → staging texture.
                let dst: ID3D11Resource = staging.cast()?;
                let src: ID3D11Resource = texture.cast()?;
                self.context.CopyResource(&dst, &src);

                // Map staging texture for CPU read.
                // Pass `staging` directly — ID3D11Texture2D implements
                // CanInto<ID3D11Resource> so Param<ID3D11Resource> is satisfied.
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("Map (staging) failed")?;

                // RowPitch may include hardware padding; copy row-by-row.
                let row_pitch = mapped.RowPitch as usize;
                let row_bytes = self.width as usize * 4;
                let mut data = vec![0u8; (self.width * self.height * 4) as usize];
                let src_slice = std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    row_pitch * self.height as usize,
                );
                for row in 0..self.height as usize {
                    let s = row * row_pitch;
                    let d = row * row_bytes;
                    data[d..d + row_bytes]
                        .copy_from_slice(&src_slice[s..s + row_bytes]);
                }

                self.context.Unmap(staging, 0);
                Ok(data)
            }
        }
    }

    /// Create a D3D11 device on the adapter at `adapter_index` and attach a
    /// `IDXGIOutputDuplication` for `output_index`.
    fn init_dxgi(
        adapter_index: u32,
        output_index: u32,
    ) -> Result<(
        ID3D11Device,
        ID3D11DeviceContext,
        IDXGIOutputDuplication,
        u32,
        u32,
    )> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().context("CreateDXGIFactory1 failed")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapters1(adapter_index)
                .context("EnumAdapters1 failed — adapter index out of range?")?;

            // D3D_DRIVER_TYPE_UNKNOWN is required when an explicit adapter is given.
            let feature_levels = [
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let adapter_base: IDXGIAdapter = adapter.cast()?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                Some(&adapter_base),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice failed")?;
            let device = device.context("D3D11CreateDevice returned null device")?;
            let context =
                context.context("D3D11CreateDevice returned null context")?;

            let (dup, w, h) =
                recreate_duplication(&device, adapter_index, output_index)?;
            Ok((device, context, dup, w, h))
        }
    }

    /// (Re-)create just the `IDXGIOutputDuplication` from an existing device.
    /// Called both at startup and after `DXGI_ERROR_ACCESS_LOST`.
    fn recreate_duplication(
        device: &ID3D11Device,
        adapter_index: u32,
        output_index: u32,
    ) -> Result<(IDXGIOutputDuplication, u32, u32)> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().context("CreateDXGIFactory1 failed")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapters1(adapter_index)
                .context("EnumAdapters1 failed")?;
            let output: IDXGIOutput = adapter
                .EnumOutputs(output_index)
                .context("EnumOutputs failed — output index out of range?")?;
            let output1: IDXGIOutput1 = output.cast()?;

            let duplication =
                output1.DuplicateOutput(device).context("DuplicateOutput failed")?;

            // Retrieve display dimensions from the duplication descriptor.
            // DXGI_OUTDUPL_DESC.ModeDesc is a DXGI_MODE_DESC with Width/Height.
            let dup_desc = duplication.GetDesc();
            let width = dup_desc.ModeDesc.Width;
            let height = dup_desc.ModeDesc.Height;

            Ok((duplication, width, height))
        }
    }
}

// ---------------------------------------------------------------------------
// Re-export or stub
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub use imp::Capturer;

/// Non-Windows stub — compiles but always returns an error at runtime.
#[cfg(not(windows))]
pub struct Capturer;

#[cfg(not(windows))]
impl Capturer {
    pub fn new(_config: CaptureConfig) -> Result<Self> {
        anyhow::bail!("DXGI Desktop Duplication is only supported on Windows")
    }
    pub fn grab(&mut self) -> Result<Option<RawFrame>> {
        anyhow::bail!("DXGI Desktop Duplication is only supported on Windows")
    }
    pub fn width(&self) -> u32 {
        0
    }
    pub fn height(&self) -> u32 {
        0
    }
}
