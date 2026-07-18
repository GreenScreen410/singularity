// Windows-only probe: which window configuration breaks DX12 swapchain
// creation on this machine? Tries several winit window styles against a
// DX12-only wgpu instance and reports pass/fail for each.
// Run (from WSL works too): cargo build --target x86_64-pc-windows-gnu --example dx12_probe

use std::sync::Arc;
use winit::{
    dpi::PhysicalSize,
    event_loop::EventLoop,
    window::{Fullscreen, WindowBuilder, WindowLevel},
};

fn probe(event_loop: &EventLoop<()>, name: &str, build: impl FnOnce() -> WindowBuilder) {
    let window = match build().build(event_loop) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            println!("{name}: window creation failed: {e}");
            return;
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("adapter");
        let info = adapter.get_info();
        let (device, _queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default(), None),
        )
        .expect("device");
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );
        format!("{:?} {:?} size={}x{}", info.backend, info.name, size.width, size.height)
    }));
    match result {
        Ok(d) => println!("{name}: OK ({d})"),
        Err(_) => println!("{name}: FAIL (configure panicked)"),
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    probe(&event_loop, "1 plain visible 800x600", || {
        WindowBuilder::new().with_inner_size(PhysicalSize::new(800, 600))
    });
    probe(&event_loop, "2 plain hidden", || {
        WindowBuilder::new()
            .with_inner_size(PhysicalSize::new(800, 600))
            .with_visible(false)
    });
    probe(&event_loop, "3 undecorated+topmost visible", || {
        WindowBuilder::new()
            .with_inner_size(PhysicalSize::new(800, 600))
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
    });
    probe(&event_loop, "4 fullscreen borderless visible", || {
        WindowBuilder::new()
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
    });
    probe(&event_loop, "5 fullscreen borderless hidden (app combo)", || {
        WindowBuilder::new()
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
            .with_visible(false)
    });
    // manual "fullscreen": undecorated window sized/positioned over the
    // monitor by hand, no winit fullscreen state at all
    let monitor = event_loop.primary_monitor().unwrap();
    let msize = monitor.size();
    let mpos = monitor.position();
    probe(&event_loop, "6 manual monitor-cover visible", || {
        WindowBuilder::new()
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(msize)
            .with_position(mpos)
    });
    probe(&event_loop, "7 manual monitor-cover hidden", || {
        WindowBuilder::new()
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(msize)
            .with_position(mpos)
            .with_visible(false)
    });
    println!("probe done");
}
