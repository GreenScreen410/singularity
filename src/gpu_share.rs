// D3D12 side of the zero-copy capture path.
//
// wgpu runs on DX12; the capture runs on D3D11. We create shared textures on
// the D3D12 side (HEAP_FLAG_SHARED + ALLOW_SIMULTANEOUS_ACCESS, which is the
// keyed-mutex-free sharing mode), export NT handles for the capture thread to
// open, and wrap the very same resources as wgpu textures so the desktop
// frame never leaves the GPU. The capture thread GPU-copies each frame into
// one of these and publishes just the buffer index.

use crate::GPU_BUFFERS;
use windows::core::Interface;
use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device, ID3D12Resource, D3D12_HEAP_FLAG_SHARED, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
    D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_SAMPLE_DESC};

pub struct GpuShare {
    pub textures: Vec<wgpu::Texture>,
    pub handles: [isize; GPU_BUFFERS],
    _resources: Vec<ID3D12Resource>, // keeps our COM references alive
}

pub fn create(device: &wgpu::Device, width: u32, height: u32) -> Result<GpuShare, String> {
    // Pull the raw ID3D12Device out of wgpu. None => not the DX12 backend.
    let raw = unsafe {
        device.as_hal::<wgpu::hal::api::Dx12, _, _>(|hal| {
            hal.map(|d| d.raw_device().as_mut_ptr() as *mut std::ffi::c_void)
        })
    }
    .flatten()
    .ok_or("wgpu is not running on the DX12 backend")?;
    let device12: ID3D12Device = unsafe { ID3D12Device::from_raw_borrowed(&raw) }
        .ok_or("null D3D12 device")?
        .clone();

    let mut textures = Vec::with_capacity(GPU_BUFFERS);
    let mut resources = Vec::with_capacity(GPU_BUFFERS);
    let mut handles = [0isize; GPU_BUFFERS];
    for slot in handles.iter_mut() {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        // sRGB variant so sampling linearizes exactly like the CPU upload
        // path did; CopyResource across same-family formats is legal.
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: width as u64,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
        };
        let mut res: Option<ID3D12Resource> = None;
        unsafe {
            device12.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_SHARED,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut res,
            )
        }
        .map_err(|e| format!("CreateCommittedResource: {e}"))?;
        let res = res.ok_or("CreateCommittedResource returned nothing")?;

        let handle = unsafe { device12.CreateSharedHandle(&res, None, GENERIC_ALL.0, None) }
            .map_err(|e| format!("CreateSharedHandle: {e}"))?;
        *slot = handle.0 as isize;

        // Wrap the same resource as a wgpu texture. The clone's reference is
        // handed over to wgpu-hal (from_raw takes ownership of one ref).
        let ptr = res.clone().into_raw();
        let hal_tex = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                d3d12::ComPtr::from_raw(ptr as *mut _),
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                1,
                1,
            )
        };
        let tex = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("shared desktop"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };
        textures.push(tex);
        resources.push(res);
    }
    Ok(GpuShare {
        textures,
        handles,
        _resources: resources,
    })
}
