//! Per-pixel-alpha rendering for a native, nonactivating taskbar window.
//!
//! The HWND must use WS_EX_NOREDIRECTIONBITMAP. DirectComposition blends this
//! premultiplied BGRA surface with the real taskbar; text keeps its own alpha.
//! No layered-window global opacity or undocumented backdrop API is involved.

use std::{marker::PhantomData, rc::Rc};
use windows::Win32::Foundation::{E_UNEXPECTED, HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIDevice1,
    IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::{Error, Interface, Result};

/// UI-thread-owned rendering resources. A failed resize/present requires the
/// caller to discard the surface and its target-bound resources before recovery.
pub struct CompositionSurface {
    // Keep target resources before their devices for deterministic COM release.
    render_target: Option<ID2D1RenderTarget>,
    composition_target: IDCompositionTarget,
    visual: IDCompositionVisual,
    swap_chain: IDXGISwapChain1,
    composition_device: IDCompositionDevice,
    context: ID3D11DeviceContext,
    _d3d_device: ID3D11Device,
    factory: ID2D1Factory,
    width: u32,
    height: u32,
    dpi: f32,
    software_fallback: bool,
    _thread_affinity: PhantomData<Rc<()>>,
}

impl CompositionSurface {
    /// Requires a live HWND and an initialized COM apartment on this UI thread.
    pub unsafe fn new(hwnd: HWND, dpi: f32) -> Result<Self> {
        unsafe {
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client)?;
            let width = (client.right - client.left).max(1) as u32;
            let height = (client.bottom - client.top).max(1) as u32;
            let dpi = valid_dpi(dpi);

            let (d3d_device, context, software_fallback) =
                match create_device(D3D_DRIVER_TYPE_HARDWARE) {
                    Ok((device, context)) => (device, context, false),
                    Err(_) => {
                        // Windows 11 supports shared WARP surfaces. This keeps the
                        // widget recoverable while a hardware driver is unavailable.
                        let (device, context) = create_device(D3D_DRIVER_TYPE_WARP)?;
                        (device, context, true)
                    }
                };
            let dxgi_device: IDXGIDevice = d3d_device.cast()?;
            if let Ok(device1) = dxgi_device.cast::<IDXGIDevice1>() {
                // One queued frame suffices for infrequent telemetry updates.
                let _ = device1.SetMaximumFrameLatency(1);
            }
            let adapter = dxgi_device.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;
            let description = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            };
            let swap_chain =
                dxgi_factory.CreateSwapChainForComposition(&d3d_device, &description, None)?;
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let render_target = create_target(&factory, &swap_chain, dpi)?;
            clear_target(&render_target)?;
            // Establish a transparent first frame before attaching the visual.
            swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;

            let composition_device: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let composition_target = composition_device.CreateTargetForHwnd(hwnd, true)?;
            let visual = composition_device.CreateVisual()?;
            visual.SetContent(&swap_chain)?;
            composition_target.SetRoot(&visual)?;
            composition_device.Commit()?;

            Ok(Self {
                render_target: Some(render_target),
                composition_target,
                visual,
                swap_chain,
                composition_device,
                context,
                _d3d_device: d3d_device,
                factory,
                width,
                height,
                dpi,
                software_fallback,
                _thread_affinity: PhantomData,
            })
        }
    }

    /// The reference and any brushes/bitmaps made from it must not survive resize.
    /// Do not call this after resize returned an error; rebuild the surface first.
    pub fn target(&self) -> &ID2D1RenderTarget {
        self.render_target
            .as_ref()
            .expect("rebuild CompositionSurface after a failed resize")
    }

    /// Use this factory for path geometries/stroke styles drawn on this target.
    /// Objects from an unrelated Direct2D factory can produce WRONG_FACTORY.
    pub fn factory(&self) -> &ID2D1Factory {
        &self.factory
    }

    /// Release the caller's target-bound COM resources before calling this.
    /// Width/height are physical client pixels; text is subsequently laid out in DIPs.
    pub unsafe fn resize(&mut self, width: u32, height: u32, dpi: f32) -> Result<()> {
        unsafe {
            let width = width.max(1);
            let height = height.max(1);
            self.set_dpi(dpi);
            if width == self.width && height == self.height && self.render_target.is_some() {
                return Ok(());
            }

            // DXGI requires every reference to the old back buffer to be gone.
            // ClearState also removes implicit immediate-context view bindings.
            self.render_target = None;
            self.context.ClearState();
            self.context.Flush();
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
            let target = create_target(&self.factory, &self.swap_chain, self.dpi)?;
            clear_target(&target)?;
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            self.width = width;
            self.height = height;
            self.render_target = Some(target);
            Ok(())
        }
    }

    /// Call after a successful target.EndDraw(). No compositor commit is needed
    /// for each frame: the visual already references this swap-chain object.
    pub unsafe fn present(&self) -> Result<()> {
        unsafe {
            if self.render_target.is_none() {
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
            // Return DXGI device-removed/reset errors to the renderer's recovery path.
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()
        }
    }

    pub unsafe fn set_dpi(&mut self, dpi: f32) {
        unsafe {
            self.dpi = valid_dpi(dpi);
            if let Some(target) = &self.render_target {
                target.SetDpi(self.dpi, self.dpi);
            }
        }
    }

    /// Diagnostic information, not a claim about measured resource consumption.
    pub fn uses_software_fallback(&self) -> bool {
        self.software_fallback
    }
}

impl Drop for CompositionSurface {
    fn drop(&mut self) {
        unsafe {
            // Detach before releasing the HWND target, allowing immediate device
            // recreation on the same window after a display-driver reset.
            let _ = self
                .composition_target
                .SetRoot(None::<&IDCompositionVisual>);
            let _ = self.visual.SetContent(None::<&windows::core::IUnknown>);
            let _ = self.composition_device.Commit();
            self.render_target = None;
            self.context.ClearState();
            self.context.Flush();
        }
    }
}

unsafe fn create_device(
    driver_type: D3D_DRIVER_TYPE,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        Ok((
            device.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?,
            context.ok_or_else(|| Error::from_hresult(E_UNEXPECTED))?,
        ))
    }
}

unsafe fn create_target(
    factory: &ID2D1Factory,
    swap_chain: &IDXGISwapChain1,
    dpi: f32,
) -> Result<ID2D1RenderTarget> {
    unsafe {
        let surface: IDXGISurface = swap_chain.GetBuffer(0)?;
        let target = factory.CreateDxgiSurfaceRenderTarget(
            &surface,
            &D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            },
        )?;
        // Subpixel ClearType assumes an opaque background; grayscale preserves
        // correct alpha edges while the taskbar is visible through the surface.
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        Ok(target)
    }
}

unsafe fn clear_target(target: &ID2D1RenderTarget) -> Result<()> {
    unsafe {
        target.BeginDraw();
        target.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }));
        target.EndDraw(None, None)
    }
}

fn valid_dpi(dpi: f32) -> f32 {
    if dpi.is_finite() && dpi > 0.0 {
        dpi
    } else {
        96.0
    }
}
