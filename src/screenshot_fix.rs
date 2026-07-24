// PrtScn clipboard fix (Windows).
//
// The overlay is excluded from screen capture (it has to be, or its own
// capture would feed back), so Print Screen produces a hole-less image. The
// OS offers no per-consumer exclusion, so instead of fighting the capture we
// fix its output: when a full-virtual-desktop-sized bitmap lands on the
// clipboard right after PrtScn, we re-render the hole over that very image
// (it IS the clean desktop) and put the composited version back. The user
// keeps pressing PrtScn like always; paste just contains the hole now.
//
// Scope limits, on purpose: only full-screen copies are touched (a region
// snip's crop offset is unknowable), and files saved to disk by Win+PrtScn
// are left alone (silently editing user files is not our place).

use crate::Uniforms;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

const CF_DIB: u32 = 8;

/// What the compositor needs to know about one pane.
pub struct PaneShot {
    /// monitor rect in virtual-desktop pixels
    pub pos: (i32, i32),
    pub size: (u32, u32),
    pub uniforms: Uniforms,
}

struct Dib {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    bpp: u32,
    stride: usize,
    pixel_offset: usize,
    bottom_up: bool,
}

fn read_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn read_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn parse_dib(bytes: Vec<u8>) -> Option<Dib> {
    if bytes.len() < 40 {
        return None;
    }
    let header_size = read_u32(&bytes, 0) as usize;
    let width = read_i32(&bytes, 4);
    let raw_height = read_i32(&bytes, 8);
    let bpp = u16::from_le_bytes([bytes[14], bytes[15]]) as u32;
    let compression = read_u32(&bytes, 16);
    // BI_RGB = 0, BI_BITFIELDS = 3 (bitfield masks follow the header)
    if width <= 0 || raw_height == 0 || !(bpp == 32 || bpp == 24) {
        return None;
    }
    let masks = if compression == 3 {
        12
    } else if compression == 0 {
        0
    } else {
        return None;
    };
    let clr_used = read_u32(&bytes, 32) as usize;
    let pixel_offset = header_size + masks + clr_used * 4;
    let stride = ((width as usize * bpp as usize / 8) + 3) & !3;
    let height = raw_height.unsigned_abs();
    if bytes.len() < pixel_offset + stride * height as usize {
        return None;
    }
    Some(Dib {
        bytes,
        width: width as u32,
        height,
        bpp,
        stride,
        pixel_offset,
        bottom_up: raw_height > 0,
    })
}

fn clipboard_dib() -> Option<Vec<u8>> {
    // The producer (snipping tool, or whatever wrote the clipboard) may
    // still hold it or be mid-write when the sequence number ticks, so a
    // couple of short retries make the first attempt reliable.
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
        let got = unsafe {
            if OpenClipboard(None).is_err() {
                continue;
            }
            let out = (|| {
                let h: HANDLE = GetClipboardData(CF_DIB).ok()?;
                let hg = HGLOBAL(h.0);
                let ptr = GlobalLock(hg) as *const u8;
                if ptr.is_null() {
                    return None;
                }
                let len = GlobalSize(hg);
                let data = std::slice::from_raw_parts(ptr, len).to_vec();
                let _ = GlobalUnlock(hg);
                Some(data)
            })();
            let _ = CloseClipboard();
            out
        };
        if got.is_some() {
            return got;
        }
    }
    None
}

fn write_clipboard_dib(owner: isize, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        // Prepare the replacement before touching the clipboard. Once
        // EmptyClipboard succeeds, the old screenshot is gone.
        let hg = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
            .map_err(|e| format!("GlobalAlloc: {e}"))?;
        let ptr = GlobalLock(hg) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hg);

        if let Err(e) = OpenClipboard(Some(HWND(owner as *mut _))) {
            let _ = GlobalFree(Some(hg));
            return Err(format!("OpenClipboard: {e}"));
        }
        let res = (|| {
            EmptyClipboard().map_err(|e| format!("EmptyClipboard: {e}"))?;
            SetClipboardData(CF_DIB, Some(HANDLE(hg.0)))
                .map_err(|e| format!("SetClipboardData: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();
        if res.is_err() {
            // Ownership transfers to the system only after SetClipboardData
            // succeeds.
            let _ = GlobalFree(Some(hg));
        }
        res
    }
}

/// Extract one pane's rect from the DIB as tight top-down BGRA.
fn crop_bgra(dib: &Dib, x0: i32, y0: i32, w: u32, h: u32, vx: i32, vy: i32) -> Vec<u8> {
    let px = (x0 - vx) as usize;
    let py = (y0 - vy) as usize;
    let bypp = (dib.bpp / 8) as usize;
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let src_row = if dib.bottom_up {
            dib.height as usize - 1 - (py + y)
        } else {
            py + y
        };
        let src = dib.pixel_offset + src_row * dib.stride + px * bypp;
        let dst = y * w as usize * 4;
        if dib.bpp == 32 {
            out[dst..dst + w as usize * 4]
                .copy_from_slice(&dib.bytes[src..src + w as usize * 4]);
        } else {
            for x in 0..w as usize {
                let s = src + x * 3;
                let d = dst + x * 4;
                out[d] = dib.bytes[s];
                out[d + 1] = dib.bytes[s + 1];
                out[d + 2] = dib.bytes[s + 2];
                out[d + 3] = 255;
            }
        }
    }
    out
}

/// Write a tight top-down BGRA block back into the DIB at the pane rect.
fn paste_bgra(dib: &mut Dib, data: &[u8], x0: i32, y0: i32, w: u32, h: u32, vx: i32, vy: i32) {
    let px = (x0 - vx) as usize;
    let py = (y0 - vy) as usize;
    let bypp = (dib.bpp / 8) as usize;
    for y in 0..h as usize {
        let dst_row = if dib.bottom_up {
            dib.height as usize - 1 - (py + y)
        } else {
            py + y
        };
        let dst = dib.pixel_offset + dst_row * dib.stride + px * bypp;
        let src = y * w as usize * 4;
        if dib.bpp == 32 {
            dib.bytes[dst..dst + w as usize * 4]
                .copy_from_slice(&data[src..src + w as usize * 4]);
        } else {
            for x in 0..w as usize {
                let s = src + x * 4;
                let d = dst + x * 3;
                dib.bytes[d] = data[s];
                dib.bytes[d + 1] = data[s + 1];
                dib.bytes[d + 2] = data[s + 2];
            }
        }
    }
}

/// Render the hole over one pane's clipboard crop and return tight BGRA.
#[allow(clippy::too_many_arguments)]
fn render_pane_shot(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    format: wgpu::TextureFormat,
    shot: &PaneShot,
    background_bgra: &[u8],
) -> Result<Vec<u8>, String> {
    let (w, h) = shot.size;
    let extent = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    // clipboard crop as the "desktop" texture (same sRGB format as capture)
    let bg_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shot background"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &bg_tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        background_bgra,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        extent,
    );

    let mut u = shot.uniforms;
    u.has_desktop = 1.0; // the clipboard image is the desktop
    let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shot uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(&u));
    let bind_group = crate::make_bind_group(device, layout, &ubuf, &bg_tex, sampler);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shot target"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let padded = (w as usize * 4).next_multiple_of(256);
    let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shot readback"),
        size: (padded * h as usize) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shot pass"),
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
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &out_buf,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(h),
            },
        },
        extent,
    );
    queue.submit(Some(enc.finish()));

    let slice = out_buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv()
        .map_err(|_| "map channel closed".to_string())?
        .map_err(|e| format!("map failed: {e:?}"))?;
    let mapped = slice.get_mapped_range();
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let s = y * padded;
        let d = y * w as usize * 4;
        out[d..d + w as usize * 4].copy_from_slice(&mapped[s..s + w as usize * 4]);
    }
    Ok(out)
}

/// If the clipboard holds a full-virtual-desktop screenshot, composite the
/// hole into it and write it back. Returns true when it fixed something.
#[allow(clippy::too_many_arguments)]
pub fn try_fix(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    format: wgpu::TextureFormat,
    clipboard_owner: isize,
    virtual_origin: (i32, i32),
    virtual_size: (u32, u32),
    shots: &[PaneShot],
) -> Result<bool, String> {
    let Some(raw) = clipboard_dib() else {
        eprintln!("screenshot: no DIB on the clipboard");
        return Ok(false);
    };
    let Some(mut dib) = parse_dib(raw) else {
        eprintln!("screenshot: unsupported DIB format; leaving it alone");
        return Ok(false);
    };
    if (dib.width, dib.height) != virtual_size {
        eprintln!(
            "screenshot: size {}x{} != virtual desktop {}x{}; leaving it alone",
            dib.width, dib.height, virtual_size.0, virtual_size.1
        );
        return Ok(false);
    }
    for shot in shots {
        let bg = crop_bgra(
            &dib,
            shot.pos.0,
            shot.pos.1,
            shot.size.0,
            shot.size.1,
            virtual_origin.0,
            virtual_origin.1,
        );
        let rendered = render_pane_shot(
            device, queue, pipeline, layout, sampler, format, shot, &bg,
        )?;
        paste_bgra(
            &mut dib,
            &rendered,
            shot.pos.0,
            shot.pos.1,
            shot.size.0,
            shot.size.1,
            virtual_origin.0,
            virtual_origin.1,
        );
    }
    write_clipboard_dib(clipboard_owner, &dib.bytes)?;
    Ok(true)
}
