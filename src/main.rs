// Singularity - a drifting black hole over the live desktop.
// Desktop captured via Windows.Graphics.Capture; our own window is excluded
// from capture so we don't feed back into ourselves. Falls back to a test
// pattern until the first frame.
//
// GUI subsystem in release: no console window. Debug builds keep the console
// so capture/overlay/gpu diagnostics are visible - build with plain
// `cargo build` and run the target/debug exe to troubleshoot.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex};
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopWindowTarget},
    keyboard::{Key, NamedKey},
    window::{Window, WindowBuilder, WindowLevel},
};

#[cfg(windows)]
#[path = "capture_windows.rs"]
mod capture;
#[cfg(target_os = "macos")]
#[path = "capture_macos.rs"]
mod capture;
#[cfg(windows)]
mod gpu_share;
#[cfg(windows)]
mod screenshot_fix;

/// Number of shared GPU textures in the zero-copy ring (Windows).
pub const GPU_BUFFERS: usize = 3;

// Platform-neutral shared frame state, filled by the capture thread and read
// by the render loop. Two delivery modes: CPU (data holds the frame bytes)
// and, on Windows, zero-copy GPU (gpu_index names a shared texture instead).
#[derive(Default)]
pub struct SharedFrame {
    pub data: Vec<u8>, // BGRA8, width*height*4, tightly packed (CPU mode)
    pub width: u32,
    pub height: u32,
    pub version: u64,
    /// Some(i): the newest frame lives in shared GPU texture i, not in data
    pub gpu_index: Option<usize>,
    /// NT handles for the shared textures, set by the render side once ready
    pub gpu_handles: Option<[isize; GPU_BUFFERS]>,
    pub gpu_size: (u32, u32),
    /// either side sets this on failure -> both stay on the CPU path
    pub gpu_disabled: bool,
    /// which monitor to capture (0-based); the capture thread restarts its
    /// session when this changes
    pub monitor_index: usize,
    /// bumped by the capture thread on every session (re)start so the render
    /// side knows to renegotiate GPU sharing
    pub epoch: u64,
}
pub type Shared = Arc<Mutex<SharedFrame>>;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms {
    pub resolution: [f32; 2],
    pub time: f32,
    pub has_desktop: f32,
    pub look: [f32; 14], // temp incl roll inner outer opac dopp beam gain contr wind speed expo star
    pub hole_radius: f32,
    pub center: [f32; 2], // hole centre in uv, computed on the CPU
    pub _pad: [f32; 3],
}

const DEFAULT_SIZE: f32 = 0.09; // shadow radius, fraction of screen height
const DEFAULT_DRIFT_SPEED: f32 = 1.0;
const DEFAULT_DRIFT_X: f32 = 0.20;
const DEFAULT_DRIFT_Y: f32 = 0.14;
const PRESET_FADE_SEC: f32 = 1.0; // crossfade time when switching looks

// The 8 looks from the original's tuner (ParamSpec.swift), resolved against
// its defaults:      temp     incl   roll   inner outer opac  dopp  beam gain contr wind speed expo  star
const PRESETS: [(&str, [f32; 14]); 8] = [
    ("Inferno",       [ 5500.0, 1.50,  0.35, 1.8,  8.0, 0.90, 0.60, 2.5, 2.2, 1.6, 7.0, 5.0, 1.40, 0.0]),
    ("Gargantua",     [ 4500.0, 1.52,  0.10, 2.2,  7.0, 0.85, 0.35, 2.0, 1.4, 0.5, 7.0, 5.0, 1.20, 0.0]),
    ("Quasar",        [15000.0, 1.30,  0.35, 3.0, 14.0, 0.35, 1.00, 4.0, 1.2, 1.3, 8.0, 5.0, 0.80, 0.0]),
    ("M87* donut",    [ 3800.0, 0.55, -0.30, 2.2,  6.0, 0.45, 0.90, 3.5, 1.6, 0.4, 3.0, 2.5, 1.10, 0.0]),
    ("Blazar",        [18000.0, 1.05,  0.55, 3.0, 16.0, 0.30, 1.00, 5.0, 1.0, 1.5, 9.0, 6.0, 0.75, 0.0]),
    ("Face-on ember", [ 6500.0, 0.30,  0.00, 3.0, 10.0, 0.50, 0.80, 2.5, 1.0, 1.1, 7.0, 5.0, 1.00, 0.0]),
    ("Pure lens",     [ 5500.0, 1.50,  0.35, 1.8,  8.0, 0.00, 1.00, 2.5, 0.0, 1.6, 7.0, 5.0, 1.00, 0.6]),
    ("Zen",           [ 7000.0, 1.45,  0.15, 3.5,  7.0, 0.40, 0.50, 2.0, 0.5, 0.3, 3.0, 1.5, 0.70, 0.0]),
];
const DEFAULT_PRESET: usize = 1; // Gargantua

// config-file keys for the presets, in PRESETS order
const PRESET_KEYS: [&str; 8] = [
    "inferno", "gargantua", "quasar", "m87", "blazar", "ember", "lens", "zen",
];

/// Values parsed from singularity.toml; None = key absent/commented out.
#[derive(Clone, Copy, PartialEq, Default)]
struct FileCfg {
    preset: Option<usize>,
    size: Option<f32>,
    drift_speed: Option<f32>,
    drift_x: Option<f32>,
    drift_y: Option<f32>,
    fps: Option<u32>,
    idle_minutes: Option<f32>,
    check_updates: Option<u32>,
    pin_x: Option<f32>,
    pin_y: Option<f32>,
    monitor: Option<usize>, // 0 = all, otherwise 1-based
    fix_screenshots: Option<u32>,
}

fn parse_config(text: &str) -> FileCfg {
    let mut cfg = FileCfg::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "preset" => cfg.preset = PRESET_KEYS.iter().position(|p| p.eq_ignore_ascii_case(v)),
            "size" => cfg.size = v.parse().ok(),
            "drift_speed" => cfg.drift_speed = v.parse().ok(),
            "drift_x" => cfg.drift_x = v.parse().ok(),
            "drift_y" => cfg.drift_y = v.parse().ok(),
            "fps" => cfg.fps = v.parse().ok(),
            "idle_minutes" => cfg.idle_minutes = v.parse().ok(),
            "check_updates" => cfg.check_updates = v.parse().ok(),
            "pin_x" => cfg.pin_x = v.parse().ok(),
            "pin_y" => cfg.pin_y = v.parse().ok(),
            "monitor" => cfg.monitor = v.parse().ok(),
            "fix_screenshots" => cfg.fix_screenshots = v.parse().ok(),
            _ => {}
        }
    }
    cfg
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("singularity.toml"))
}

// only written from the tray's "Open Config File", which Linux doesn't build
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const DEFAULT_CONFIG: &str = "\
# Singularity settings. Saved changes apply within a second, no restart.
# Remove the leading # to activate a line; active values take priority
# over the tray menu until they change again.

# Disk look: inferno | gargantua | quasar | m87 | blazar | ember | lens | zen
#preset = gargantua

# Shadow radius as a fraction of screen height.
# Tray Small / Medium / Large = 0.06 / 0.09 / 0.14
#size = 0.09

# Wander speed multiplier and horizontal/vertical range (0 to 0.5).
#drift_speed = 1.0
#drift_x = 0.20
#drift_y = 0.14

# Frame rate cap (saves battery). 0 = uncapped (monitor refresh rate).
#fps = 0

# Screensaver mode: appear only after this many minutes without any
# keyboard/mouse input, and vanish on the first input. 0 = always visible.
#idle_minutes = 0

# Once a day, ask GitHub whether a newer release exists and show it in the
# tray menu. This is the only network access the app ever makes.
# 0 = never check. Takes effect on restart.
#check_updates = 1

# Placement: hold Ctrl+Shift anywhere and the hole follows your mouse;
# release to pin it. Tray > Position > Auto drift resumes wandering.
# Or pin a fixed spot here (0 to 1, fraction of the combined desktop).
#pin_x = 0.5
#pin_y = 0.5

# Monitors: 0 = roam across all of them (default), N = stay on monitor N.
# Also in the tray menu.
#monitor = 0

# Print Screen produces a hole-less image (the overlay must exclude itself
# from capture, or it would capture itself forever). When this is on, a
# full-screen screenshot landing on the clipboard right after PrtScn gets
# the hole composited back in. 0 = leave the clipboard alone.
#fix_screenshots = 1
";

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn ensure_config_file(path: &std::path::Path) {
    if !path.exists() {
        if let Err(e) = std::fs::write(path, DEFAULT_CONFIG) {
            eprintln!("config: cannot create {}: {e}", path.display());
        }
    }
}

/// One overlay window on one monitor: its surface, its capture session and
/// its share of the zero-copy machinery. The hole itself lives in State in
/// virtual-desktop coordinates; every pane renders it from its own viewpoint,
/// which is what lets it roam seamlessly across monitor boundaries.
struct Pane {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    #[cfg_attr(not(windows), allow(dead_code))]
    mon_index: usize,
    mon_pos: PhysicalPosition<i32>,
    mon_size: PhysicalSize<u32>,
    shared: Shared,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    desktop_texture: wgpu::Texture,
    tex_size: (u32, u32),
    last_version: u64,
    has_desktop: bool,
    visible: bool,
    #[cfg(windows)]
    last_epoch: u64,
    #[cfg(windows)]
    gpu_share: Option<gpu_share::GpuShare>,
    #[cfg(windows)]
    gpu_bind_groups: Vec<wgpu::BindGroup>,
    #[cfg(windows)]
    gpu_attempted: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    gpu_current: Option<usize>,
}

struct State {
    instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    panes: Vec<Pane>,
    start: std::time::Instant,
    look_from: [f32; 14],
    look_to: [f32; 14],
    fade_start: std::time::Instant,
    hole_radius: f32,
    drift_speed: f32,
    drift_x: f32,
    drift_y: f32,
    fps: u32,          // 0 = uncapped (vsync only)
    idle_minutes: f32, // 0 = always visible; >0 = appear after this much idle
    appear_start: Option<std::time::Instant>, // grow-in animation anchor
    // hole placement in virtual-desktop pixels: the roam box is the bounding
    // box of the selected monitors, the centre is smoothed toward its target
    roam_pos: [f64; 2],
    roam_size: [f64; 2],
    primary_h: f64, // hole size reference so it stays constant across panes
    center_px: [f64; 2],
    pinned_px: Option<[f64; 2]>,
    last_center_tick: std::time::Instant,
}

impl State {
    async fn new(
        target: &EventLoopWindowTarget<()>,
        monitors: &[winit::monitor::MonitorHandle],
        selection: Option<usize>, // None = every monitor
    ) -> State {
        // Windows must be DX12 (the zero-copy capture path shares D3D12
        // textures); macOS is Metal; elsewhere let Vulkan/GL race.
        #[cfg(windows)]
        let backends = wgpu::Backends::DX12;
        #[cfg(target_os = "macos")]
        let backends = wgpu::Backends::METAL;
        #[cfg(not(any(windows, target_os = "macos")))]
        let backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });

        // hidden window + surface shells for every selected monitor
        let shells = make_shells(&instance, target, monitors, selection);

        // On hybrid-GPU laptops the capture's D3D11 device lives on the
        // DEFAULT adapter (usually the iGPU) while HighPerformance would pick
        // the dGPU, and shared textures cannot cross adapters. Prefer the
        // wgpu adapter whose LUID matches the default adapter so the
        // zero-copy path works; fall back to the normal request.
        #[cfg(windows)]
        let adapter = {
            let mut chosen = None;
            if let Some(luid) = default_adapter_luid() {
                for a in instance.enumerate_adapters(wgpu::Backends::DX12) {
                    if adapter_luid(&a) == Some(luid) {
                        eprintln!("render: using capture-matched adapter: {}", a.get_info().name);
                        chosen = Some(a);
                        break;
                    }
                }
            }
            match chosen {
                Some(a) => a,
                None => instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::HighPerformance,
                        compatible_surface: shells.first().map(|s| &s.surface),
                        force_fallback_adapter: false,
                    })
                    .await
                    .expect("no suitable GPU adapter"),
            }
        };
        #[cfg(not(windows))]
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: shells.first().map(|s| &s.surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("no suitable GPU adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .expect("failed to create device");

        // one pipeline for all panes; the format is the first surface's
        // preference (universally Bgra8UnormSrgb on Windows/macOS)
        let format = shells
            .first()
            .map(|s| {
                let caps = s.surface.get_capabilities(&adapter);
                caps.formats
                    .iter()
                    .copied()
                    .find(|f| f.is_srgb())
                    .unwrap_or(caps.formats[0])
            })
            .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::include_wgsl!("singularity.wgsl"));
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let primary_h = monitors
            .first()
            .map(|m| m.size().height as f64)
            .unwrap_or(1080.0);
        let mut state = State {
            instance,
            _adapter: adapter,
            device,
            queue,
            pipeline,
            format,
            bind_group_layout,
            sampler,
            panes: Vec::new(),
            start: std::time::Instant::now(),
            look_from: PRESETS[DEFAULT_PRESET].1,
            look_to: PRESETS[DEFAULT_PRESET].1,
            fade_start: std::time::Instant::now(),
            hole_radius: DEFAULT_SIZE,
            drift_speed: DEFAULT_DRIFT_SPEED,
            drift_x: DEFAULT_DRIFT_X,
            drift_y: DEFAULT_DRIFT_Y,
            fps: 0,
            idle_minutes: 0.0,
            appear_start: None,
            roam_pos: [0.0, 0.0],
            roam_size: [1.0, 1.0],
            primary_h,
            center_px: [0.0, 0.0],
            pinned_px: None,
            last_center_tick: std::time::Instant::now(),
        };
        state.finish_panes(shells);
        state.update_roam_box();
        state.center_px = [
            state.roam_pos[0] + state.roam_size[0] * 0.5,
            state.roam_pos[1] + state.roam_size[1] * 0.5,
        ];
        state
    }

    /// Turn shells into full panes: configure the surface, create per-pane
    /// resources, start that monitor's capture, then apply the overlay styles
    /// (they must come after the swapchain exists, see main()).
    fn finish_panes(&mut self, shells: Vec<Shell>) {
        for shell in shells {
            let Shell {
                window,
                surface,
                mon_index,
                mon_pos,
                mon_size,
            } = shell;
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.format,
                width: mon_size.width.max(1),
                height: mon_size.height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(&self.device, &config);

            let uniform_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("uniforms"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let desktop_texture = create_desktop_texture(&self.device, 1, 1);
            let bind_group = make_bind_group(
                &self.device,
                &self.bind_group_layout,
                &uniform_buf,
                &desktop_texture,
                &self.sampler,
            );

            let shared: Shared = Arc::new(Mutex::new(SharedFrame {
                monitor_index: mon_index,
                ..Default::default()
            }));
            #[cfg(any(windows, target_os = "macos"))]
            capture::start(shared.clone(), mon_index);

            // overlay styles only after the swapchain exists (layered-window
            // rule); the window is still hidden at this point
            let _ = window.set_cursor_hittest(false);
            #[cfg(any(windows, target_os = "macos"))]
            set_capture_exclusion(&window, true);

            self.panes.push(Pane {
                window,
                surface,
                config,
                mon_index,
                mon_pos,
                mon_size,
                shared,
                uniform_buf,
                bind_group,
                desktop_texture,
                tex_size: (1, 1),
                last_version: 0,
                has_desktop: false,
                visible: false,
                #[cfg(windows)]
                last_epoch: 0,
                #[cfg(windows)]
                gpu_share: None,
                #[cfg(windows)]
                gpu_bind_groups: Vec::new(),
                #[cfg(windows)]
                gpu_attempted: false,
                gpu_current: None,
            });
        }
    }

    /// The union bounding box of the selected monitors, in virtual pixels.
    fn update_roam_box(&mut self) {
        let mut min = [f64::MAX, f64::MAX];
        let mut max = [f64::MIN, f64::MIN];
        for p in &self.panes {
            min[0] = min[0].min(p.mon_pos.x as f64);
            min[1] = min[1].min(p.mon_pos.y as f64);
            max[0] = max[0].max(p.mon_pos.x as f64 + p.mon_size.width as f64);
            max[1] = max[1].max(p.mon_pos.y as f64 + p.mon_size.height as f64);
        }
        if self.panes.is_empty() {
            min = [0.0, 0.0];
            max = [1920.0, 1080.0];
        }
        self.roam_pos = min;
        self.roam_size = [(max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0)];
    }

    /// Tear the panes down and rebuild them for a new monitor selection.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn set_selection(
        &mut self,
        target: &EventLoopWindowTarget<()>,
        monitors: &[winit::monitor::MonitorHandle],
        selection: Option<usize>,
    ) {
        for p in &self.panes {
            // sentinel: tells that pane's capture thread to shut down
            p.shared.lock().unwrap().monitor_index = usize::MAX;
        }
        self.panes.clear();
        let shells = make_shells(&self.instance, target, monitors, selection);
        self.finish_panes(shells);
        self.update_roam_box();
        self.center_px = [
            self.roam_pos[0] + self.roam_size[0] * 0.5,
            self.roam_pos[1] + self.roam_size[1] * 0.5,
        ];
    }

    /// Current look: smoothstep crossfade from look_from to look_to.
    fn current_look(&self) -> [f32; 14] {
        let t = (self.fade_start.elapsed().as_secs_f32() / PRESET_FADE_SEC).min(1.0);
        let e = t * t * (3.0 - 2.0 * t);
        let mut out = [0.0f32; 14];
        for i in 0..14 {
            out[i] = self.look_from[i] + (self.look_to[i] - self.look_from[i]) * e;
        }
        out
    }

    /// Global cursor position in virtual-desktop pixels.
    #[cfg(windows)]
    fn cursor_px(&self) -> Option<[f64; 2]> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p).ok()? };
        Some([p.x as f64, p.y as f64])
    }

    #[cfg(target_os = "macos")]
    fn cursor_px(&self) -> Option<[f64; 2]> {
        #[repr(C)]
        struct CGPoint {
            x: f64,
            y: f64,
        }
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFRelease(cf: *mut std::ffi::c_void);
        }
        unsafe {
            let ev = CGEventCreate(std::ptr::null());
            if ev.is_null() {
                return None;
            }
            let loc = CGEventGetLocation(ev); // global, top-left origin, points
            CFRelease(ev);
            let scale = self
                .panes
                .first()
                .map(|p| p.window.scale_factor())
                .unwrap_or(1.0);
            Some([loc.x * scale, loc.y * scale])
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn cursor_px(&self) -> Option<[f64; 2]> {
        None
    }

    fn clamp_roam(&self, p: [f64; 2]) -> [f64; 2] {
        let mx = self.roam_size[0] * 0.02;
        let my = self.roam_size[1] * 0.02;
        [
            p[0].clamp(self.roam_pos[0] + mx, self.roam_pos[0] + self.roam_size[0] - mx),
            p[1].clamp(self.roam_pos[1] + my, self.roam_pos[1] + self.roam_size[1] - my),
        ]
    }

    /// Advance the hole centre: follow the cursor while the placement hotkey
    /// is held (pinning where it lands), else glide toward the pin or the
    /// Lissajous drift path over the whole roam box.
    fn tick_center(&mut self) {
        let now = std::time::Instant::now();
        let dt = (now - self.last_center_tick).as_secs_f32().min(0.1);
        self.last_center_tick = now;

        if place_hotkey_held() {
            if let Some(p) = self.cursor_px() {
                self.pinned_px = Some(self.clamp_roam(p));
            }
        }
        let target = match self.pinned_px {
            Some(p) => p,
            None => {
                let t = self.start.elapsed().as_secs_f32() * 0.12 * self.drift_speed;
                let l = lissa(t);
                [
                    self.roam_pos[0]
                        + self.roam_size[0] * (0.5 + l[0] as f64 * self.drift_x as f64),
                    self.roam_pos[1]
                        + self.roam_size[1] * (0.5 + l[1] as f64 * self.drift_y as f64),
                ]
            }
        };
        let k = (1.0 - (-dt * 6.0).exp()) as f64;
        self.center_px[0] += (target[0] - self.center_px[0]) * k;
        self.center_px[1] += (target[1] - self.center_px[1]) * k;
    }

    /// Screensaver grow-in: 0 -> 1 over ~2 s after the overlay appears from
    /// idle, so the hole swells out of nothing instead of popping in.
    fn appear_factor(&self) -> f32 {
        match self.appear_start {
            Some(t0) => {
                let x = (t0.elapsed().as_secs_f32() / 2.0).min(1.0);
                x * x * (3.0 - 2.0 * x)
            }
            None => 1.0,
        }
    }

    // only reachable from the tray menu, which Linux doesn't build
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn set_preset(&mut self, idx: usize) {
        self.look_from = self.current_look();
        self.look_to = PRESETS[idx].1;
        self.fade_start = std::time::Instant::now();
    }

    fn resize_pane(&mut self, i: usize, new_size: PhysicalSize<u32>) {
        // Skip no-op reconfigures: the window is layered after startup and a
        // DX12 swapchain rebuild on a layered window fails.
        let device = &self.device;
        let pane = &mut self.panes[i];
        if new_size.width > 0
            && new_size.height > 0
            && (new_size.width != pane.config.width || new_size.height != pane.config.height)
        {
            pane.config.width = new_size.width;
            pane.config.height = new_size.height;
            pane.surface.configure(device, &pane.config);
        }
    }

    /// Per-pane update: ingest the newest capture frame and write uniforms.
    fn update_pane(&mut self, i: usize) {
        // zero-copy maintenance wants disjoint borrows; do it via free fns
        #[cfg(windows)]
        {
            let device = &self.device;
            let layout = &self.bind_group_layout;
            let sampler = &self.sampler;
            let pane = &mut self.panes[i];
            pane_gpu_maintenance(device, layout, sampler, pane);
        }

        {
        let queue = &self.queue;
        let device = &self.device;
        let layout = &self.bind_group_layout;
        let sampler = &self.sampler;
        let pane = &mut self.panes[i];

        // pull the latest desktop frame for this pane
        let frame = {
            let g = pane.shared.lock().unwrap();
            if g.version != pane.last_version && g.width > 0 && g.height > 0 {
                pane.last_version = g.version;
                match g.gpu_index {
                    Some(gi) => {
                        pane.gpu_current = Some(gi);
                        pane.has_desktop = true;
                        None
                    }
                    None => {
                        pane.gpu_current = None;
                        Some((g.width, g.height, g.data.clone()))
                    }
                }
            } else {
                None
            }
        };
        if let Some((w, h, data)) = frame {
            if (w, h) != pane.tex_size {
                pane.desktop_texture = create_desktop_texture(device, w, h);
                pane.bind_group = make_bind_group(
                    device,
                    layout,
                    &pane.uniform_buf,
                    &pane.desktop_texture,
                    sampler,
                );
                pane.tex_size = (w, h);
            }
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &pane.desktop_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            pane.has_desktop = true;
        }
        } // release the pane borrow before the shared-self uniform math

        let u = self.pane_uniforms(i);
        self.queue
            .write_buffer(&self.panes[i].uniform_buf, 0, bytemuck::bytes_of(&u));
    }

    /// The uniform block for one pane at this instant. Also used by the
    /// screenshot compositor, which renders the same hole over a clipboard
    /// image instead of the live capture.
    fn pane_uniforms(&self, i: usize) -> Uniforms {
        let pane = &self.panes[i];
        Uniforms {
            resolution: [pane.config.width as f32, pane.config.height as f32],
            time: self.start.elapsed().as_secs_f32(),
            has_desktop: if pane.has_desktop { 1.0 } else { 0.0 },
            look: self.current_look(),
            // the hole keeps one physical size everywhere: radius is relative
            // to the primary monitor's height, rescaled per pane
            hole_radius: self.hole_radius * self.appear_factor()
                * (self.primary_h as f32 / pane.mon_size.height.max(1) as f32),
            center: [
                ((self.center_px[0] - pane.mon_pos.x as f64)
                    / pane.mon_size.width.max(1) as f64) as f32,
                ((self.center_px[1] - pane.mon_pos.y as f64)
                    / pane.mon_size.height.max(1) as f64) as f32,
            ],
            _pad: [0.0; 3],
        }
    }

    fn render_pane(&mut self, i: usize) -> Result<(), wgpu::SurfaceError> {
        let pipeline = &self.pipeline;
        let device = &self.device;
        let queue = &self.queue;
        let pane = &mut self.panes[i];
        let frame = pane.surface.get_current_texture()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline);
            // zero-copy mode samples the shared texture's bind group directly
            let bg: &wgpu::BindGroup;
            #[cfg(windows)]
            {
                bg = match pane.gpu_current {
                    Some(gi) if gi < pane.gpu_bind_groups.len() => &pane.gpu_bind_groups[gi],
                    _ => &pane.bind_group,
                };
            }
            #[cfg(not(windows))]
            {
                bg = &pane.bind_group;
            }
            pass.set_bind_group(0, bg, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

/// A freshly created hidden window + surface on one monitor, not yet a Pane.
struct Shell {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    #[cfg_attr(not(windows), allow(dead_code))]
    mon_index: usize,
    mon_pos: PhysicalPosition<i32>,
    mon_size: PhysicalSize<u32>,
}

/// Manual borderless "fullscreen" per monitor: undecorated topmost windows
/// sized and placed by hand. winit's Fullscreen::Borderless state makes DXGI
/// swapchain creation fail with DXGI_ERROR_INVALID_CALL (verified by
/// examples/dx12_probe.rs), while a manually monitor-sized window works.
fn make_shells(
    instance: &wgpu::Instance,
    target: &EventLoopWindowTarget<()>,
    monitors: &[winit::monitor::MonitorHandle],
    selection: Option<usize>,
) -> Vec<Shell> {
    let indices: Vec<usize> = match selection {
        Some(i) if i < monitors.len() => vec![i],
        _ => (0..monitors.len()).collect(),
    };
    let mut shells = Vec::new();
    for i in indices {
        let m = &monitors[i];
        let (pos, size) = (m.position(), m.size());
        let window = Arc::new(
            WindowBuilder::new()
                .with_title("Singularity")
                .with_decorations(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_inner_size(size)
                .with_position(pos)
                .with_visible(false) // hidden until its first frame is ready
                .build(target)
                .unwrap(),
        );
        let surface = instance.create_surface(window.clone()).unwrap();
        shells.push(Shell {
            window,
            surface,
            mon_index: i,
            mon_pos: pos,
            mon_size: size,
        });
    }
    shells
}

/// Zero-copy upkeep for one pane: renegotiate after a capture session restart
/// and set up the shared textures once the capture size is known.
#[cfg(windows)]
fn pane_gpu_maintenance(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    pane: &mut Pane,
) {
    let ep = pane.shared.lock().unwrap().epoch;
    if ep != pane.last_epoch {
        pane.last_epoch = ep;
        pane.gpu_share = None;
        pane.gpu_bind_groups.clear();
        pane.gpu_attempted = false;
        pane.gpu_current = None;
        pane.has_desktop = false;
    }
    if pane.gpu_attempted {
        return;
    }
    let (w, h, already, disabled) = {
        let g = pane.shared.lock().unwrap();
        (g.width, g.height, g.gpu_handles.is_some(), g.gpu_disabled)
    };
    if disabled {
        pane.gpu_attempted = true;
        return;
    }
    if already || w == 0 || h == 0 {
        return; // no frame yet
    }
    pane.gpu_attempted = true;
    match gpu_share::create(device, w, h) {
        Ok(share) => {
            pane.gpu_bind_groups = share
                .textures
                .iter()
                .map(|t| make_bind_group(device, layout, &pane.uniform_buf, t, sampler))
                .collect();
            let mut g = pane.shared.lock().unwrap();
            g.gpu_size = (w, h);
            g.gpu_handles = Some(share.handles);
            pane.gpu_share = Some(share);
            eprintln!(
                "render: shared GPU textures ready for monitor {} ({w}x{h})",
                pane.mon_index + 1
            );
        }
        Err(e) => {
            eprintln!("render: GPU sharing unavailable ({e}); using CPU path");
            pane.shared.lock().unwrap().gpu_disabled = true;
        }
    }
}

fn create_desktop_texture(device: &wgpu::Device, w: u32, h: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("desktop"),
        size: wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform_buf: &wgpu::Buffer,
    texture: &wgpu::Texture,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Make our overlay window invisible to screen capture (so it doesn't feed
/// back into the desktop capture). Minimal user32 FFI to avoid pinning a
/// specific `windows` crate version.
#[cfg(windows)]
fn set_capture_exclusion(window: &Window, on: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    const WDA_NONE: u32 = 0x0;
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;
    #[link(name = "user32")]
    extern "system" {
        fn SetWindowDisplayAffinity(hwnd: isize, dw_affinity: u32) -> i32;
    }
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            unsafe {
                SetWindowDisplayAffinity(
                    h.hwnd.get(),
                    if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE },
                );
            }
        }
    }
}

/// macOS equivalent: NSWindowSharingNone removes the window from all screen
/// capture (ScreenCaptureKit respects it), preventing self-capture feedback.
#[cfg(target_os = "macos")]
fn set_capture_exclusion(window: &Window, on: bool) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let sharing: usize = if on { 0 } else { 1 }; // None / ReadOnly (default)
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::AppKit(h) = handle.as_raw() {
            unsafe {
                let view = h.ns_view.as_ptr() as *mut AnyObject;
                let ns_window: *mut AnyObject = msg_send![&*view, window];
                if !ns_window.is_null() {
                    let _: () = msg_send![&*ns_window, setSharingType: sharing];
                }
            }
        }
    }
}

/// Unit Lissajous wander, the same incommensurate sines the shader used.
fn lissa(t: f32) -> [f32; 2] {
    [
        0.75 * (t * 0.37).sin() + 0.25 * (t * 0.83 + 1.0).sin(),
        0.70 * (t * 0.54 + 2.1).sin() + 0.30 * (t * 1.07).sin(),
    ]
}

/// Placement hotkey: both Ctrl and Shift held, observed globally without
/// intercepting anything.
#[cfg(windows)]
fn place_hotkey_held() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
    unsafe {
        (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0)
            && (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0)
    }
}

#[cfg(target_os = "macos")]
fn place_hotkey_held() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state: i32) -> u64;
    }
    const CTRL: u64 = 0x0004_0000;
    const SHIFT: u64 = 0x0002_0000;
    let f = unsafe { CGEventSourceFlagsState(0) }; // combined session state
    f & CTRL != 0 && f & SHIFT != 0
}

#[cfg(not(any(windows, target_os = "macos")))]
fn place_hotkey_held() -> bool {
    false
}

// ------------------------------ update check -------------------------------
// Once a day, ask the GitHub API for the latest release tag using the OS's
// bundled curl (no HTTP dependency in the app). If it is newer, the tray
// gains an "Update available" entry that opens the releases page. Opt out
// with check_updates = 0 in the config file.

#[cfg(any(windows, target_os = "macos"))]
const RELEASES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/latest");

#[cfg(any(windows, target_os = "macos"))]
fn fetch_latest_version() -> Option<String> {
    const API: &str =
        "https://api.github.com/repos/GreenScreen410/singularity/releases/latest";
    let mut cmd = std::process::Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    cmd.args(["-s", "--max-time", "10", "-H", "User-Agent: singularity", API]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok()?;
    let body = String::from_utf8_lossy(&out.stdout);
    // extract "tag_name": "x.y.z" without pulling in a JSON parser
    let rest = &body[body.find("\"tag_name\"")?..];
    let after = rest[rest.find(':')? + 1..].trim_start().strip_prefix('"')?;
    Some(after[..after.find('"')?].trim_start_matches('v').to_string())
}

#[cfg(any(windows, target_os = "macos"))]
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    let l = parse(latest);
    !l.is_empty() && l > parse(current)
}

#[cfg(any(windows, target_os = "macos"))]
fn start_update_check(found: Arc<Mutex<Option<String>>>) {
    std::thread::spawn(move || loop {
        if let Some(latest) = fetch_latest_version() {
            let current = env!("CARGO_PKG_VERSION");
            if is_newer(&latest, current) {
                eprintln!("update: {latest} available (running {current})");
                *found.lock().unwrap() = Some(latest);
                return; // one notification per run is enough
            }
            eprintln!("update: up to date ({current})");
        }
        std::thread::sleep(std::time::Duration::from_secs(24 * 60 * 60));
    });
}

#[cfg(windows)]
fn open_url(url: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(0x0800_0000)
        .spawn();
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// LUID of the system default adapter: the one D3D11CreateDevice(None) and
/// therefore the capture uses. (LowPart, HighPart).
#[cfg(windows)]
fn default_adapter_luid() -> Option<(u32, i32)> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, IDXGIFactory4, DXGI_CREATE_FACTORY_FLAGS,
    };
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        Some((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
    }
}

/// LUID of a wgpu adapter (DX12 backend only).
#[cfg(windows)]
fn adapter_luid(adapter: &wgpu::Adapter) -> Option<(u32, i32)> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
    unsafe {
        adapter.as_hal::<wgpu::hal::api::Dx12, _, _>(|hal| {
            let hal = hal?;
            // hal's adapter is a windows 0.58 wrapper; bridge via raw pointer
            let raw = windows_058::core::Interface::as_raw(&**hal.raw_adapter());
            let a = IDXGIAdapter::from_raw_borrowed(&raw)?;
            let desc = a.GetDesc().ok()?;
            Some((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
        })
    }
}

/// Seconds since the last system-wide keyboard/mouse input, like a
/// screensaver would measure it.
#[cfg(windows)]
fn idle_seconds() -> f32 {
    #[repr(C)]
    struct LastInputInfo {
        cb_size: u32,
        dw_time: u32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount() -> u32;
    }
    let mut lii = LastInputInfo { cb_size: 8, dw_time: 0 };
    unsafe {
        if GetLastInputInfo(&mut lii) != 0 {
            GetTickCount().wrapping_sub(lii.dw_time) as f32 / 1000.0
        } else {
            0.0
        }
    }
}

#[cfg(target_os = "macos")]
fn idle_seconds() -> f32 {
    // kCGEventSourceStateCombinedSessionState = 0, kCGAnyInputEventType = !0
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) as f32 }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn idle_seconds() -> f32 {
    0.0
}

/// Programmatic tray icon: a black hole - dark disc with a warm ring.
#[cfg(any(windows, target_os = "macos"))]
fn tray_icon_rgba(size: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    let c = (size as f32 - 1.0) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c;
            let (r, g, b, a) = if d < 0.52 {
                (0u8, 0u8, 0u8, 255u8) // shadow
            } else if d < 0.80 {
                (255, 190, 110, 255) // photon ring / disk
            } else if d < 0.95 {
                (120, 70, 30, 160) // faint outer glow
            } else {
                (0, 0, 0, 0)
            };
            px.extend_from_slice(&[r, g, b, a]);
        }
    }
    px
}

// sub-option values, shared by the tray menu and its handler
#[cfg(any(windows, target_os = "macos"))]
const SIZES: [(&str, f32); 3] = [("Small", 0.06), ("Medium", 0.09), ("Large", 0.14)];
#[cfg(any(windows, target_os = "macos"))]
const SPEEDS: [(&str, f32); 3] = [("Slow", 0.4), ("Normal", 1.0), ("Fast", 2.2)];
#[cfg(any(windows, target_os = "macos"))]
const FPS_OPTS: [(&str, u32); 3] = [("30", 30), ("60", 60), ("Unlimited", 0)];
#[cfg(any(windows, target_os = "macos"))]
const IDLE_OPTS: [(&str, f32); 4] =
    [("Off", 0.0), ("1 min", 1.0), ("5 min", 5.0), ("10 min", 10.0)];

#[cfg(any(windows, target_os = "macos"))]
struct Tray {
    _icon: tray_icon::TrayIcon,
    menu: tray_icon::menu::Menu,
    presets: Vec<tray_icon::menu::CheckMenuItem>,
    sizes: Vec<tray_icon::menu::CheckMenuItem>,
    speeds: Vec<tray_icon::menu::CheckMenuItem>,
    fps: Vec<tray_icon::menu::CheckMenuItem>,
    idles: Vec<tray_icon::menu::CheckMenuItem>,
    positions: Vec<tray_icon::menu::CheckMenuItem>,
    monitors: Vec<tray_icon::menu::CheckMenuItem>,
    open_cfg_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

/// Build the tray icon with the preset + options menu (Windows: taskbar
/// overflow area / macOS: menu bar).
///
/// Must be called AFTER the event loop has started (StartCause::Init): on
/// macOS an NSStatusItem created before NSApplication is fully up crashes
/// AppKit with NSCGSPanic (confirmed on Monterey), and winit only sets up
/// NSApplication when the loop runs.
#[cfg(any(windows, target_os = "macos"))]
fn build_tray(monitor_labels: &[String], current_monitor: usize, pinned: bool) -> Tray {
    use tray_icon::{
        menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
        Icon, TrayIconBuilder,
    };
    let menu = Menu::new();
    let mut presets: Vec<CheckMenuItem> = Vec::new();
    for (i, (name, _)) in PRESETS.iter().enumerate() {
        let item = CheckMenuItem::new(*name, true, i == DEFAULT_PRESET, None);
        menu.append(&item).unwrap();
        presets.push(item);
    }
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    // stepped option submenus; default checked = Medium/Normal/Unlimited/Off
    let sub = |title: &str, names: &[&str], default: usize| -> Vec<CheckMenuItem> {
        let submenu = Submenu::new(title, true);
        let items: Vec<CheckMenuItem> = names
            .iter()
            .enumerate()
            .map(|(i, n)| CheckMenuItem::new(*n, true, i == default, None))
            .collect();
        for it in &items {
            submenu.append(it).unwrap();
        }
        menu.append(&submenu).unwrap();
        items
    };
    let sizes = sub("Size", &SIZES.map(|s| s.0), 1);
    let speeds = sub("Speed", &SPEEDS.map(|s| s.0), 1);
    let fps = sub("FPS", &FPS_OPTS.map(|s| s.0), 2);
    let idles = sub("Screensaver", &IDLE_OPTS.map(|s| s.0), 0);
    let positions = sub(
        "Position",
        &["Auto drift", "Pinned (Ctrl+Shift to place)"],
        if pinned { 1 } else { 0 },
    );
    let monitors = if monitor_labels.len() > 1 {
        let labels: Vec<&str> = monitor_labels.iter().map(|s| s.as_str()).collect();
        sub("Monitor", &labels, current_monitor)
    } else {
        Vec::new()
    };
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    let open_cfg = MenuItem::new("Open Config File", true, None);
    menu.append(&open_cfg).unwrap();
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&quit).unwrap();
    let icon = Icon::from_rgba(tray_icon_rgba(32), 32, 32).unwrap();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("Singularity - right-click to change look and options")
        .with_icon(icon)
        .build()
        .unwrap();
    Tray {
        _icon: tray,
        menu,
        presets,
        sizes,
        speeds,
        fps,
        idles,
        positions,
        monitors,
        open_cfg_id: open_cfg.id().clone(),
        quit_id: quit.id().clone(),
    }
}

fn main() {
    env_logger::init();

    // config-file state; read once now for startup-only decisions
    let cfg_path = config_path();
    let startup_cfg = cfg_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_config(&t))
        .unwrap_or_default();
    let mut cfg_mtime: Option<std::time::SystemTime> = None;
    let mut last_cfg_check = std::time::Instant::now();
    let mut next_frame = std::time::Instant::now();
    let mut boot_warned = false;

    let event_loop = EventLoop::new().unwrap();
    let monitors: Vec<_> = event_loop.available_monitors().collect();

    // monitor selection: 0 or absent = all monitors (the hole roams across
    // them), N = confined to monitor N
    let mon_count = monitors.len();
    let sel_from_cfg = move |m: Option<usize>| -> Option<usize> {
        match m {
            Some(0) | None => None,
            Some(n) => {
                let i = n - 1;
                if i < mon_count {
                    Some(i)
                } else {
                    None
                }
            }
        }
    };
    let mut current_sel = sel_from_cfg(startup_cfg.monitor);

    // daily update check (config: check_updates = 0 opts out, read at startup)
    #[cfg(any(windows, target_os = "macos"))]
    let update_available: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    #[cfg(any(windows, target_os = "macos"))]
    if startup_cfg.check_updates.unwrap_or(1) != 0 {
        start_update_check(update_available.clone());
    }
    #[cfg(any(windows, target_os = "macos"))]
    let mut update_item: Option<tray_icon::menu::MenuItem> = None;

    // created at StartCause::Init inside the event loop, see build_tray docs
    #[cfg(any(windows, target_os = "macos"))]
    let mut tray: Option<Tray> = None;

    let mut state = pollster::block_on(State::new(&event_loop, &monitors, current_sel));

    // apply the rest of the startup config and make hot-reload diff from it
    if let Some(i) = startup_cfg.preset {
        state.set_preset(i);
    }
    if let Some(v) = startup_cfg.size {
        state.hole_radius = v;
    }
    if let Some(v) = startup_cfg.drift_speed {
        state.drift_speed = v;
    }
    if let Some(v) = startup_cfg.drift_x {
        state.drift_x = v;
    }
    if let Some(v) = startup_cfg.drift_y {
        state.drift_y = v;
    }
    if let Some(v) = startup_cfg.fps {
        state.fps = v;
    }
    if let Some(v) = startup_cfg.idle_minutes {
        state.idle_minutes = v;
    }
    if let (Some(x), Some(y)) = (startup_cfg.pin_x, startup_cfg.pin_y) {
        // pin coordinates are fractions of the roam box
        state.pinned_px = Some([
            state.roam_pos[0] + state.roam_size[0] * x.clamp(0.0, 1.0) as f64,
            state.roam_pos[1] + state.roam_size[1] * y.clamp(0.0, 1.0) as f64,
        ]);
    }
    let mut prev_cfg = startup_cfg;

    // ui-side trackers for the event loop
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(unused))]
    let mut tray_pinned_ui = state.pinned_px.is_some();

    // PrtScn clipboard fix state
    #[cfg(windows)]
    let mut fix_screenshots = startup_cfg.fix_screenshots.unwrap_or(1) != 0;
    #[cfg(windows)]
    let mut last_prtscn: Option<std::time::Instant> = None;
    #[cfg(windows)]
    let mut last_clip_seq: u32 = unsafe {
        windows::Win32::System::DataExchange::GetClipboardSequenceNumber()
    };
    // full virtual desktop bounds (all monitors), what PrtScn captures
    #[cfg(windows)]
    let (virtual_origin, virtual_size) = {
        let mut min = (i32::MAX, i32::MAX);
        let mut max = (i32::MIN, i32::MIN);
        for m in &monitors {
            let p = m.position();
            let s = m.size();
            min.0 = min.0.min(p.x);
            min.1 = min.1.min(p.y);
            max.0 = max.0.max(p.x + s.width as i32);
            max.1 = max.1.max(p.y + s.height as i32);
        }
        if monitors.is_empty() {
            ((0, 0), (1920u32, 1080u32))
        } else {
            (min, ((max.0 - min.0) as u32, (max.1 - min.1) as u32))
        }
    };

    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop
        .run(move |event, elwt| match event {
            // NSStatusItem must be created after NSApplication is running on
            // macOS; StartCause::Init is the first moment that is true.
            Event::NewEvents(winit::event::StartCause::Init) => {
                #[cfg(any(windows, target_os = "macos"))]
                {
                    let mut labels = vec!["All monitors".to_string()];
                    labels.extend(monitors.iter().enumerate().map(|(i, m)| {
                        format!("{}: {}x{}", i + 1, m.size().width, m.size().height)
                    }));
                    let checked = match current_sel {
                        None => 0,
                        Some(i) => i + 1,
                    };
                    tray = Some(build_tray(&labels, checked, state.pinned_px.is_some()));
                }
            }
            Event::WindowEvent { event, window_id } => {
                let Some(i) = state.panes.iter().position(|p| p.window.id() == window_id)
                else {
                    return;
                };
                match event {
                    WindowEvent::CloseRequested => elwt.exit(),
                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                logical_key: Key::Named(NamedKey::Escape),
                                state: ElementState::Pressed,
                                ..
                            },
                        ..
                    } => elwt.exit(),
                    WindowEvent::Resized(new_size) => state.resize_pane(i, new_size),
                    WindowEvent::RedrawRequested => {
                        state.update_pane(i);
                        match state.render_pane(i) {
                            Ok(()) => {}
                            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                                let device = &state.device;
                                let pane = &state.panes[i];
                                pane.surface.configure(device, &pane.config);
                            }
                            Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                            Err(e) => eprintln!("render error: {e:?}"),
                        }
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                state.tick_center();

                // Visibility state machine, per pane. This must live HERE,
                // not in the render path: a hidden window gets no WM_PAINT,
                // so a render-side reveal would deadlock into invisibility.
                //
                // booted:  that pane's first capture frame arrived (or 3s)
                // wanted:  always, or only after idle_minutes without input
                let waited = state.start.elapsed().as_secs_f32();
                let idle_mode = state.idle_minutes > 0.0;
                let wanted = !idle_mode || idle_seconds() >= state.idle_minutes * 60.0;
                let any_visible = state.panes.iter().any(|p| p.visible);
                for i in 0..state.panes.len() {
                    let ready = { state.panes[i].shared.lock().unwrap().width > 0 };
                    let booted = ready || waited > 3.0;
                    if booted && !ready && !boot_warned {
                        boot_warned = true;
                        eprintln!(
                            "warning: no capture frame after {waited:.1}s - \
                             showing test pattern; check 'capture:' messages above"
                        );
                    }
                    let show = wanted && booted;
                    if show && !state.panes[i].visible {
                        if idle_mode && !any_visible {
                            // grow in from nothing when appearing out of idle
                            state.appear_start = Some(std::time::Instant::now());
                        }
                        state.update_pane(i);
                        let _ = state.render_pane(i); // pre-paint while hidden
                        let pane = &mut state.panes[i];
                        pane.window.set_visible(true);
                        // re-assert overlay traits after the hidden start
                        pane.window.set_outer_position(pane.mon_pos);
                        let _ = pane.window.request_inner_size(pane.mon_size);
                        pane.window.set_window_level(WindowLevel::AlwaysOnTop);
                        pane.visible = true;
                    } else if !show && state.panes[i].visible {
                        // any input dismisses the screensaver instantly
                        let pane = &mut state.panes[i];
                        pane.window.set_visible(false);
                        pane.visible = false;
                    }
                }

                // PrtScn clipboard fix: when a full-screen screenshot lands
                // on the clipboard shortly after PrtScn, composite the hole
                // into it (the capture exclusion keeps it out of the original)
                #[cfg(windows)]
                {
                    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
                    use windows::Win32::UI::Input::KeyboardAndMouse::{
                        GetAsyncKeyState, VK_SNAPSHOT,
                    };
                    let down = unsafe { GetAsyncKeyState(VK_SNAPSHOT.0 as i32) } as u16;
                    if down & 0x8001 != 0 {
                        last_prtscn = Some(std::time::Instant::now());
                    }
                    let seq = unsafe { GetClipboardSequenceNumber() };
                    if seq != last_clip_seq {
                        last_clip_seq = seq;
                        let recent =
                            last_prtscn.is_some_and(|t| t.elapsed().as_secs_f32() < 10.0);
                        eprintln!(
                            "screenshot: clipboard changed (prtscn recent: {recent})"
                        );
                        if fix_screenshots && recent && state.panes.iter().any(|p| p.visible) {
                            let shots: Vec<screenshot_fix::PaneShot> = state
                                .panes
                                .iter()
                                .enumerate()
                                .filter(|(_, p)| p.visible)
                                .map(|(i, p)| screenshot_fix::PaneShot {
                                    pos: (p.mon_pos.x, p.mon_pos.y),
                                    size: (p.mon_size.width, p.mon_size.height),
                                    uniforms: state.pane_uniforms(i),
                                })
                                .collect();
                            match screenshot_fix::try_fix(
                                &state.device,
                                &state.queue,
                                &state.pipeline,
                                &state.bind_group_layout,
                                &state.sampler,
                                state.format,
                                virtual_origin,
                                virtual_size,
                                &shots,
                            ) {
                                Ok(true) => {
                                    eprintln!(
                                        "screenshot: hole composited into the clipboard image"
                                    );
                                    // our own write bumped the sequence
                                    last_clip_seq =
                                        unsafe { GetClipboardSequenceNumber() };
                                    last_prtscn = None;
                                }
                                Ok(false) => {}
                                Err(e) => eprintln!("screenshot: fix failed: {e}"),
                            }
                        }
                    }
                }

                // update-check result: surface it as the first menu entry
                #[cfg(any(windows, target_os = "macos"))]
                if update_item.is_none() {
                    if let (Some(t), Some(ver)) =
                        (&tray, update_available.lock().unwrap().clone())
                    {
                        let item = tray_icon::menu::MenuItem::new(
                            format!("Update available: {ver}"),
                            true,
                            None,
                        );
                        let _ = t.menu.insert(&item, 0);
                        let _ = t
                            .menu
                            .insert(&tray_icon::menu::PredefinedMenuItem::separator(), 1);
                        update_item = Some(item);
                    }
                }
                // tray menu events arrive on a global channel; poll each tick
                #[cfg(any(windows, target_os = "macos"))]
                if let Some(t) = &tray {
                    while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                        if let Some(ui) = &update_item {
                            if &ev.id == ui.id() {
                                open_url(RELEASES_URL);
                                continue;
                            }
                        }
                        let check_one = |items: &[tray_icon::menu::CheckMenuItem], idx: usize| {
                            for (j, it) in items.iter().enumerate() {
                                it.set_checked(j == idx);
                            }
                        };
                        if ev.id == t.quit_id {
                            elwt.exit();
                        } else if ev.id == t.open_cfg_id {
                            if let Some(path) = &cfg_path {
                                ensure_config_file(path);
                                #[cfg(windows)]
                                let _ = std::process::Command::new("notepad").arg(path).spawn();
                                #[cfg(target_os = "macos")]
                                let _ = std::process::Command::new("open")
                                    .arg("-t")
                                    .arg(path)
                                    .spawn();
                            }
                        } else if let Some(idx) =
                            t.presets.iter().position(|it| it.id() == &ev.id)
                        {
                            state.set_preset(idx);
                            check_one(&t.presets, idx);
                        } else if let Some(idx) = t.sizes.iter().position(|it| it.id() == &ev.id)
                        {
                            state.hole_radius = SIZES[idx].1;
                            check_one(&t.sizes, idx);
                        } else if let Some(idx) =
                            t.speeds.iter().position(|it| it.id() == &ev.id)
                        {
                            state.drift_speed = SPEEDS[idx].1;
                            check_one(&t.speeds, idx);
                        } else if let Some(idx) = t.fps.iter().position(|it| it.id() == &ev.id) {
                            state.fps = FPS_OPTS[idx].1;
                            check_one(&t.fps, idx);
                        } else if let Some(idx) = t.idles.iter().position(|it| it.id() == &ev.id)
                        {
                            state.idle_minutes = IDLE_OPTS[idx].1;
                            check_one(&t.idles, idx);
                        } else if let Some(idx) =
                            t.positions.iter().position(|it| it.id() == &ev.id)
                        {
                            state.pinned_px = if idx == 1 {
                                Some(state.center_px)
                            } else {
                                None
                            };
                            check_one(&t.positions, idx);
                        } else if let Some(idx) =
                            t.monitors.iter().position(|it| it.id() == &ev.id)
                        {
                            let sel = if idx == 0 { None } else { Some(idx - 1) };
                            if sel != current_sel {
                                state.set_selection(elwt, &monitors, sel);
                                current_sel = sel;
                            }
                            check_one(&t.monitors, idx);
                        }
                    }
                    // placement hotkey pins the hole; mirror that in the menu
                    let now_pinned = state.pinned_px.is_some();
                    if now_pinned != tray_pinned_ui {
                        tray_pinned_ui = now_pinned;
                        for (j, it) in t.positions.iter().enumerate() {
                            it.set_checked((j == 1) == now_pinned);
                        }
                    }
                }

                // config-file hot-reload: poll mtime ~1/s, apply only fields
                // whose value changed since the last read (so the tray keeps
                // authority over anything the file doesn't change)
                if last_cfg_check.elapsed().as_secs_f32() > 1.0 {
                    last_cfg_check = std::time::Instant::now();
                    if let Some(path) = &cfg_path {
                        if let Ok(modified) =
                            std::fs::metadata(path).and_then(|m| m.modified())
                        {
                            if cfg_mtime != Some(modified) {
                                cfg_mtime = Some(modified);
                                if let Ok(text) = std::fs::read_to_string(path) {
                                    let cfg = parse_config(&text);
                                    if cfg.preset != prev_cfg.preset {
                                        if let Some(i) = cfg.preset {
                                            state.set_preset(i);
                                            #[cfg(any(windows, target_os = "macos"))]
                                            if let Some(t) = &tray {
                                                for (j, it) in t.presets.iter().enumerate() {
                                                    it.set_checked(j == i);
                                                }
                                            }
                                        }
                                    }
                                    if cfg.size != prev_cfg.size {
                                        state.hole_radius = cfg.size.unwrap_or(DEFAULT_SIZE);
                                    }
                                    if cfg.drift_speed != prev_cfg.drift_speed {
                                        state.drift_speed =
                                            cfg.drift_speed.unwrap_or(DEFAULT_DRIFT_SPEED);
                                    }
                                    if cfg.drift_x != prev_cfg.drift_x {
                                        state.drift_x = cfg.drift_x.unwrap_or(DEFAULT_DRIFT_X);
                                    }
                                    if cfg.drift_y != prev_cfg.drift_y {
                                        state.drift_y = cfg.drift_y.unwrap_or(DEFAULT_DRIFT_Y);
                                    }
                                    if cfg.fps != prev_cfg.fps {
                                        state.fps = cfg.fps.unwrap_or(0);
                                    }
                                    if cfg.idle_minutes != prev_cfg.idle_minutes {
                                        state.idle_minutes = cfg.idle_minutes.unwrap_or(0.0);
                                    }
                                    if cfg.pin_x != prev_cfg.pin_x || cfg.pin_y != prev_cfg.pin_y
                                    {
                                        state.pinned_px = match (cfg.pin_x, cfg.pin_y) {
                                            (Some(x), Some(y)) => Some([
                                                state.roam_pos[0]
                                                    + state.roam_size[0]
                                                        * x.clamp(0.0, 1.0) as f64,
                                                state.roam_pos[1]
                                                    + state.roam_size[1]
                                                        * y.clamp(0.0, 1.0) as f64,
                                            ]),
                                            _ => None,
                                        };
                                    }
                                    #[cfg(windows)]
                                    if cfg.fix_screenshots != prev_cfg.fix_screenshots {
                                        fix_screenshots =
                                            cfg.fix_screenshots.unwrap_or(1) != 0;
                                    }
                                    if cfg.monitor != prev_cfg.monitor {
                                        let sel = sel_from_cfg(cfg.monitor);
                                        if sel != current_sel {
                                            state.set_selection(elwt, &monitors, sel);
                                            current_sel = sel;
                                            #[cfg(any(windows, target_os = "macos"))]
                                            if let Some(t) = &tray {
                                                let checked = match current_sel {
                                                    None => 0,
                                                    Some(i) => i + 1,
                                                };
                                                for (j, it) in t.monitors.iter().enumerate() {
                                                    it.set_checked(j == checked);
                                                }
                                            }
                                        }
                                    }
                                    prev_cfg = cfg;
                                }
                            }
                        }
                    }
                }

                // frame pacing: hidden -> low-power 0.5s idle polling (no
                // rendering at all); uncapped -> vsync-bound Poll; capped ->
                // wake at the next frame deadline (saves battery)
                let visible = state.panes.iter().any(|p| p.visible);
                if !visible {
                    elwt.set_control_flow(ControlFlow::WaitUntil(
                        std::time::Instant::now() + std::time::Duration::from_millis(500),
                    ));
                } else if state.fps == 0 {
                    for p in state.panes.iter().filter(|p| p.visible) {
                        p.window.request_redraw();
                    }
                    elwt.set_control_flow(ControlFlow::Poll);
                } else {
                    let now = std::time::Instant::now();
                    if now >= next_frame {
                        next_frame =
                            now + std::time::Duration::from_secs_f64(1.0 / state.fps as f64);
                        for p in state.panes.iter().filter(|p| p.visible) {
                            p.window.request_redraw();
                        }
                    }
                    elwt.set_control_flow(ControlFlow::WaitUntil(next_frame));
                }
            }
            _ => {}
        })
        .unwrap();
}
