// Windows desktop capture via Windows.Graphics.Capture (windows-capture crate).
// Runs on its own thread. Preferred path: GPU-copy each frame into a shared
// D3D12 texture (zero CPU round trip); falls back to CPU readback if the
// render side could not set up sharing (non-DX12 backend, mismatched adapter,
// old drivers).

use crate::{Shared, GPU_BUFFERS};
use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device1, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
    D3D11_FENCE_FLAG_NONE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

/// D3D11 side of the zero-copy path: the shared textures opened from the
/// render side's NT handles, plus a fence so we only publish a buffer index
/// after the GPU actually finished the copy.
struct GpuPath {
    textures: Vec<ID3D11Texture2D>,
    ctx4: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    event: isize, // raw HANDLE value; HANDLE itself is not Send
    fence_value: u64,
    next: usize,
    size: (u32, u32),
}

fn open_gpu_path(frame: &Frame, handles: [isize; GPU_BUFFERS], size: (u32, u32)) -> Result<GpuPath, String> {
    let device = frame.device();
    let device1: ID3D11Device1 = device.cast().map_err(|e| format!("no ID3D11Device1: {e}"))?;
    let mut textures = Vec::with_capacity(GPU_BUFFERS);
    for h in handles {
        let tex: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(HANDLE(h as *mut _)) }
            .map_err(|e| format!("OpenSharedResource1: {e}"))?;
        textures.push(tex);
    }
    let device5: ID3D11Device5 = device.cast().map_err(|e| format!("no ID3D11Device5: {e}"))?;
    let mut fence: Option<ID3D11Fence> = None;
    unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }
        .map_err(|e| format!("CreateFence: {e}"))?;
    let fence = fence.ok_or("CreateFence returned nothing")?;
    let ctx4: ID3D11DeviceContext4 = frame
        .device_context()
        .cast()
        .map_err(|e| format!("no ID3D11DeviceContext4: {e}"))?;
    let event =
        unsafe { CreateEventW(None, false, false, None) }.map_err(|e| format!("CreateEventW: {e}"))?;
    Ok(GpuPath {
        textures,
        ctx4,
        fence,
        event: event.0 as isize,
        fence_value: 0,
        next: 0,
        size,
    })
}

struct Handler {
    shared: Shared,
    scratch: Vec<u8>,
    got_first: bool,
    gpu: Option<GpuPath>,
    gpu_failed: bool,
    monitor_index: usize, // which monitor this session captures
}

impl Handler {
    fn disable_gpu(&mut self, why: &str) {
        eprintln!("capture: GPU sharing unavailable ({why}); using CPU path");
        self.gpu = None;
        self.gpu_failed = true;
        self.shared.lock().unwrap().gpu_disabled = true;
    }
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = (Shared, usize);
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            shared: ctx.flags.0,
            scratch: Vec::new(),
            got_first: false,
            gpu: None,
            gpu_failed: false,
            monitor_index: ctx.flags.1,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // a monitor switch was requested: end this session so the outer loop
        // can start a new one on the right monitor
        if self.shared.lock().unwrap().monitor_index != self.monitor_index {
            capture_control.stop();
            return Ok(());
        }
        let width = frame.width();
        let height = frame.height();
        if !self.got_first {
            eprintln!("capture: first frame arrived ({width}x{height})");
            self.got_first = true;
        }

        // ---- zero-copy path ----
        if !self.gpu_failed {
            if self.gpu.is_none() {
                // the render side publishes NT handles once it has created
                // the shared textures (sized from our first CPU frames)
                let handles = {
                    let g = self.shared.lock().unwrap();
                    if g.gpu_disabled {
                        self.gpu_failed = true;
                        None
                    } else if g.gpu_size == (width, height) {
                        g.gpu_handles
                    } else {
                        None
                    }
                };
                if let Some(h) = handles {
                    match open_gpu_path(frame, h, (width, height)) {
                        Ok(gp) => {
                            eprintln!("capture: zero-copy GPU path active");
                            self.gpu = Some(gp);
                        }
                        Err(e) => self.disable_gpu(&e),
                    }
                }
            }
            if let Some(gp) = &mut self.gpu {
                if gp.size != (width, height) {
                    // monitor resolution changed; shared textures are stale
                    self.disable_gpu("capture size changed");
                } else {
                    let i = gp.next;
                    gp.next = (i + 1) % GPU_BUFFERS;
                    unsafe {
                        frame
                            .device_context()
                            .CopyResource(&gp.textures[i], frame.as_raw_texture());
                        // wait for the copy before publishing, so the D3D12
                        // side never samples a half-written frame
                        gp.fence_value += 1;
                        let v = gp.fence_value;
                        let event = HANDLE(gp.event as *mut _);
                        if gp.ctx4.Signal(&gp.fence, v).is_ok() {
                            if gp.fence.GetCompletedValue() < v
                                && gp.fence.SetEventOnCompletion(v, event).is_ok()
                            {
                                WaitForSingleObject(event, 100);
                            }
                        } else {
                            frame.device_context().Flush();
                        }
                    }
                    let mut g = self.shared.lock().unwrap();
                    if !g.data.is_empty() {
                        g.data = Vec::new(); // CPU staging no longer needed
                    }
                    g.width = width;
                    g.height = height;
                    g.gpu_index = Some(i);
                    g.version = g.version.wrapping_add(1);
                    return Ok(());
                }
            }
        }

        // ---- CPU fallback path ----
        let buffer = frame.buffer()?;
        // Depad rows into our reusable scratch buffer -> tight width*4 stride.
        let bytes = buffer.as_nopadding_buffer(&mut self.scratch);

        let mut g = self.shared.lock().unwrap();
        if g.data.len() != bytes.len() {
            g.data.resize(bytes.len(), 0);
        }
        g.data.copy_from_slice(bytes);
        g.width = width;
        g.height = height;
        g.gpu_index = None;
        g.version = g.version.wrapping_add(1);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        eprintln!("capture: session closed");
        Ok(())
    }
}

fn run(
    monitor_index: usize,
    cursor: CursorCaptureSettings,
    border: DrawBorderSettings,
    shared: Shared,
) -> Result<(), String> {
    // windows-capture indices are 1-based; fall back to the primary monitor
    let monitor = Monitor::from_index(monitor_index + 1)
        .or_else(|_| Monitor::primary())
        .map_err(|e| format!("no monitor: {e:?}"))?;
    let settings = Settings::new(
        monitor,
        cursor,
        border,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (shared, monitor_index),
    );
    // Blocking: runs the capture message loop on this thread until closed.
    Handler::start(settings).map_err(|e| format!("{e:?}"))
}

/// Spawn a background thread that captures the selected monitor forever,
/// restarting the session whenever shared.monitor_index changes.
/// Preferred: cursor excluded (the real cursor is OS-drawn on top of the
/// overlay, so a captured copy would ghost near the hole) and no yellow
/// capture-indicator border. Both toggles are unsupported on some older
/// Windows 10 builds, so fall back progressively if starting fails.
pub fn start(shared: Shared) {
    std::thread::spawn(move || loop {
        let idx = {
            // fresh session: reset frame + GPU-sharing negotiation state
            let mut g = shared.lock().unwrap();
            g.width = 0;
            g.height = 0;
            g.data = Vec::new();
            g.gpu_index = None;
            g.gpu_handles = None;
            g.gpu_disabled = false;
            g.epoch = g.epoch.wrapping_add(1);
            g.monitor_index
        };
        let attempts: [(CursorCaptureSettings, DrawBorderSettings, &str); 3] = [
            (
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::WithoutBorder,
                "cursor excluded, no border",
            ),
            (
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::Default,
                "cursor excluded, default border",
            ),
            (
                CursorCaptureSettings::Default,
                DrawBorderSettings::Default,
                "default settings",
            ),
        ];
        let mut ran = false;
        for (cursor, border, label) in attempts {
            eprintln!("capture: starting monitor {} ({label})", idx + 1);
            match run(idx, cursor, border, shared.clone()) {
                Ok(()) => {
                    ran = true;
                    break;
                }
                Err(e) => eprintln!("capture: {label} failed: {e}"),
            }
        }
        if !ran {
            eprintln!("capture: all attempts failed");
            return;
        }
        // if the target monitor is unchanged, the session ended for real
        if shared.lock().unwrap().monitor_index == idx {
            eprintln!("capture: session ended");
            return;
        }
    });
}
