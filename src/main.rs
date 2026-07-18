// Singularity — Stage 2
// Renders the drifting black-hole shader over the LIVE desktop, captured via
// Windows.Graphics.Capture. Our own window is excluded from capture so we don't
// feed back into ourselves. Falls back to a test pattern until the first frame.

use std::sync::{Arc, Mutex};
use winit::{
    dpi::PhysicalSize,
    event::{ElementState, Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Fullscreen, Window, WindowBuilder, WindowLevel},
};

#[cfg(windows)]
mod capture;

// Platform-neutral shared frame buffer, filled by the capture thread (Windows)
// and read by the render loop. Stays empty on non-Windows -> test pattern.
#[derive(Default)]
pub struct SharedFrame {
    pub data: Vec<u8>, // BGRA8, width*height*4, tightly packed
    pub width: u32,
    pub height: u32,
    pub version: u64,
}
pub type Shared = Arc<Mutex<SharedFrame>>;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    resolution: [f32; 2],
    time: f32,
    has_desktop: f32,
    look: [f32; 14], // temp incl roll inner outer opac dopp beam gain contr wind speed expo star
    hole_radius: f32,
    _pad: f32,
}

const HOLE_RADIUS: f32 = 0.09; // shadow radius, fraction of screen height
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

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    desktop_texture: wgpu::Texture,
    tex_size: (u32, u32),
    shared: Shared,
    last_version: u64,
    has_desktop: bool,
    shown: bool,
    start: std::time::Instant,
    look_from: [f32; 14],
    look_to: [f32; 14],
    fade_start: std::time::Instant,
}

impl State {
    async fn new(window: Arc<Window>, shared: Shared) -> State {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("no suitable GPU adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .expect("failed to create device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

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

        let desktop_texture = create_desktop_texture(&device, 1, 1);
        let bind_group = make_bind_group(
            &device,
            &bind_group_layout,
            &uniform_buf,
            &desktop_texture,
            &sampler,
        );

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
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        State {
            window,
            surface,
            device,
            queue,
            config,
            size,
            pipeline,
            uniform_buf,
            bind_group_layout,
            bind_group,
            sampler,
            desktop_texture,
            tex_size: (1, 1),
            shared,
            last_version: 0,
            has_desktop: false,
            shown: false,
            start: std::time::Instant::now(),
            look_from: PRESETS[DEFAULT_PRESET].1,
            look_to: PRESETS[DEFAULT_PRESET].1,
            fade_start: std::time::Instant::now(),
        }
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

    fn set_preset(&mut self, idx: usize) {
        self.look_from = self.current_look();
        self.look_to = PRESETS[idx].1;
        self.fade_start = std::time::Instant::now();
    }

    fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn update(&mut self) {
        // Pull the latest desktop frame. Copy out of the lock so we don't hold
        // it during GPU upload. (Extra copy; Stage 4 will remove it.)
        let frame = {
            let g = self.shared.lock().unwrap();
            if g.version != self.last_version && g.width > 0 && g.height > 0 {
                Some((g.width, g.height, g.version, g.data.clone()))
            } else {
                None
            }
        };
        if let Some((w, h, ver, data)) = frame {
            if (w, h) != self.tex_size {
                self.desktop_texture = create_desktop_texture(&self.device, w, h);
                self.bind_group = make_bind_group(
                    &self.device,
                    &self.bind_group_layout,
                    &self.uniform_buf,
                    &self.desktop_texture,
                    &self.sampler,
                );
                self.tex_size = (w, h);
            }
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.desktop_texture,
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
            self.last_version = ver;
            self.has_desktop = true;
        }

        let u = Uniforms {
            resolution: [self.config.width as f32, self.config.height as f32],
            time: self.start.elapsed().as_secs_f32(),
            has_desktop: if self.has_desktop { 1.0 } else { 0.0 },
            look: self.current_look(),
            hole_radius: HOLE_RADIUS,
            _pad: 0.0,
        };
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));
    }

    fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
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
fn exclude_from_capture(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;
    #[link(name = "user32")]
    extern "system" {
        fn SetWindowDisplayAffinity(hwnd: isize, dw_affinity: u32) -> i32;
    }
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            unsafe {
                SetWindowDisplayAffinity(h.hwnd.get(), WDA_EXCLUDEFROMCAPTURE);
            }
        }
    }
}

/// Programmatic tray icon: a black hole — dark disc with a warm ring.
#[cfg(windows)]
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

fn main() {
    env_logger::init();

    let shared: Shared = Arc::new(Mutex::new(SharedFrame::default()));

    #[cfg(windows)]
    capture::start(shared.clone());

    // ---- tray icon with preset menu (작업표시줄 ^ 영역) ----
    #[cfg(windows)]
    let (_tray, preset_items, quit_id) = {
        use tray_icon::{
            menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
            Icon, TrayIconBuilder,
        };
        let menu = Menu::new();
        let mut items: Vec<CheckMenuItem> = Vec::new();
        for (i, (name, _)) in PRESETS.iter().enumerate() {
            let item = CheckMenuItem::new(*name, true, i == DEFAULT_PRESET, None);
            menu.append(&item).unwrap();
            items.push(item);
        }
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        let quit = MenuItem::new("종료", true, None);
        menu.append(&quit).unwrap();
        let icon = Icon::from_rgba(tray_icon_rgba(32), 32, 32).unwrap();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Singularity — 우클릭으로 모양 변경")
            .with_icon(icon)
            .build()
            .unwrap();
        (tray, items, quit.id().clone())
    };

    let event_loop = EventLoop::new().unwrap();
    let window = Arc::new(
        WindowBuilder::new()
            .with_title("Singularity")
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
            .with_visible(false) // stay hidden until the first frame is ready
            .build(&event_loop)
            .unwrap(),
    );

    // Click-through: mouse events fall through to whatever is underneath.
    let _ = window.set_cursor_hittest(false);

    #[cfg(windows)]
    exclude_from_capture(&window);

    let mut state = pollster::block_on(State::new(window.clone(), shared.clone()));

    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop
        .run(move |event, elwt| match event {
            Event::WindowEvent { event, window_id } if window_id == state.window.id() => {
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
                    WindowEvent::Resized(new_size) => state.resize(new_size),
                    WindowEvent::RedrawRequested => {
                        state.update();
                        match state.render() {
                            Ok(()) => {}
                            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                                let s = state.size;
                                state.resize(s);
                            }
                            Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                            Err(e) => eprintln!("render error: {e:?}"),
                        }
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                // Reveal must live HERE, not in render(): a hidden window gets
                // no WM_PAINT, so render() never runs while hidden and a
                // render-side reveal deadlocks into an invisible app. Wait for
                // the first capture frame (or 3s), paint once while still
                // hidden, then show — no white flash, no test-pattern flash.
                if !state.shown {
                    let ready = { shared.lock().unwrap().width > 0 };
                    let waited = state.start.elapsed().as_secs_f32();
                    if ready || waited > 3.0 {
                        if !ready {
                            eprintln!(
                                "warning: no capture frame after {waited:.1}s — \
                                 showing test pattern; check 'capture:' messages above"
                            );
                        }
                        state.update();
                        let _ = state.render(); // pre-paint the hidden surface
                        state.window.set_visible(true);
                        // re-assert overlay traits after the hidden start
                        state
                            .window
                            .set_fullscreen(Some(Fullscreen::Borderless(None)));
                        state.window.set_window_level(WindowLevel::AlwaysOnTop);
                        state.shown = true;
                        eprintln!("overlay: revealed");
                    }
                }
                // tray menu events arrive on a global channel; poll each tick
                #[cfg(windows)]
                while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                    if ev.id == quit_id {
                        elwt.exit();
                    } else if let Some(idx) =
                        preset_items.iter().position(|it| it.id() == &ev.id)
                    {
                        state.set_preset(idx);
                        for (j, it) in preset_items.iter().enumerate() {
                            it.set_checked(j == idx);
                        }
                    }
                }
                state.window.request_redraw();
            }
            _ => {}
        })
        .unwrap();
}
