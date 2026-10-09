//! wgpu state for the player: one full-screen pass running `shader.wgsl`.

use super::source::{Frame, Layout};
use crate::stitch::{Lens, M3};
use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use std::sync::Arc;
use winit::window::Window;

pub const RS_STEPS_MAX: usize = 33;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LensU {
    r: [[f32; 4]; 3],
    k: [f32; 4],
    k5p: [f32; 4],
    fc: [f32; 4],
    a0: [f32; 4],
    a1: [f32; 4],
    lim: [f32; 4],
    calib: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    user: [[f32; 4]; 3],
    screen: [f32; 4],
    misc: [f32; 4],
    lenses: [LensU; 2],
    views: [[f32; 4]; RS_STEPS_MAX * 3],
}

/// What the shader needs each frame besides the pictures.
pub struct View {
    /// view_from_screen-ray rotation of the user's free look.
    pub user: M3,
    /// Tangent of half the vertical field of view (or the planet zoom).
    pub zoom: f32,
    pub projection: u32,
    /// rig_from_view at instants spread over the readout.
    pub rig_from_view: Vec<M3>,
}

struct Plane {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    /// Per texture slot: Y plane, UV plane.
    slots: [(Plane, Plane); 2],
    masks: [Plane; 2],
    bind_group: wgpu::BindGroup,
    lens_u: [LensU; 2],
}

impl Gpu {
    pub fn new(window: Arc<Window>, lenses: &[Lens]) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface unsupported by adapter")?;
        // The shader produces display-encoded values: write them to a plain
        // 8-bit surface untouched. A float surface would be read as linear
        // light, so only fall back to an sRGB one, decoding in the shader.
        let caps = surface.get_capabilities(&adapter);
        let plain = [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Rgba8Unorm];
        if let Some(f) = caps.formats.iter().find(|f| plain.contains(f)) {
            config.format = *f;
        } else if let Some(f) = caps.formats.iter().find(|f| f.is_srgb()) {
            config.format = *f;
        }
        eprintln!("surface format {:?} (offered {:?})", config.format, caps.formats);
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("stitch"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: binding < 6 },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(2),
                tex_entry(3),
                tex_entry(4),
                tex_entry(5),
                tex_entry(6),
                tex_entry(7),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("stitch"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(config.format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let blank = |device: &wgpu::Device, f| plane(device, 2, 2, f);
        let slots = [
            (blank(&device, wgpu::TextureFormat::R8Unorm), blank(&device, wgpu::TextureFormat::Rg8Unorm)),
            (blank(&device, wgpu::TextureFormat::R8Unorm), blank(&device, wgpu::TextureFormat::Rg8Unorm)),
        ];
        let masks = [0, 1].map(|i| {
            let l = &lenses[i];
            let [w, h] = l.calib_dim;
            let p = plane(&device, w as u32, h as u32, wgpu::TextureFormat::R8Unorm);
            let bytes: Vec<u8> = l.valid.iter().map(|&v| if v { 255 } else { 0 }).collect();
            write(&queue, &p.texture, &bytes, w as u32, h as u32, 1);
            p
        });
        let lens_u = [0, 1].map(|i| lens_uniform(&lenses[i]));
        let bind_group = bind(&device, &layout, &uniforms, &sampler, &slots, &masks);

        Ok(Gpu { surface, device, queue, config, pipeline, layout, sampler, uniforms, slots, masks, bind_group, lens_u })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Uploads a frame's pictures and points each lens at its part of them.
    pub fn upload(&mut self, frame: &Frame, lenses: &[Lens], tracks: &[u32]) {
        let mut rebind = false;
        for (slot, pic) in frame.pictures.iter().enumerate().take(2) {
            let (y, uv) = &mut self.slots[slot];
            if y.texture.width() != pic.width || y.texture.height() != pic.height {
                *y = plane(&self.device, pic.width, pic.height, wgpu::TextureFormat::R8Unorm);
                *uv = plane(&self.device, pic.width / 2, pic.height / 2, wgpu::TextureFormat::Rg8Unorm);
                rebind = true;
            }
            write(&self.queue, &y.texture, pic.y(), pic.width, pic.height, 1);
            write(&self.queue, &uv.texture, pic.uv(), pic.width / 2, pic.height / 2, 2);
        }
        if rebind {
            self.bind_group = bind(&self.device, &self.layout, &self.uniforms, &self.sampler, &self.slots, &self.masks);
        }
        for (i, lens) in lenses.iter().enumerate().take(2) {
            let track = tracks[lens.input];
            let (slot, pic) = match frame.layout {
                Layout::SideBySide => (0, &frame.pictures[0]),
                Layout::PerTrack => (track as usize, &frame.pictures[track as usize]),
            };
            let cell = if frame.layout == Layout::SideBySide { pic.height } else { pic.width } as f64;
            let s = cell / lens.calib_dim[0] as f64;
            let off_x = if frame.layout == Layout::SideBySide { track as f64 * cell } else { 0.0 };
            let (w, h) = (pic.width as f64, pic.height as f64);
            let a = &lens.calib_to_picture;
            let u = &mut self.lens_u[i];
            u.a0 = [(s * a.a[0][0] / w) as f32, (s * a.a[0][1] / w) as f32, ((s * a.t[0] + off_x) / w) as f32, lens.mirror as u8 as f32];
            u.a1 = [(s * a.a[1][0] / h) as f32, (s * a.a[1][1] / h) as f32, (s * a.t[1] / h) as f32, slot as f32];
        }
    }

    pub fn render(&mut self, view: &View) -> Result<()> {
        let mut u = Uniforms::zeroed();
        u.user = rows(view.user);
        u.screen = [self.config.width as f32, self.config.height as f32, view.zoom, view.projection as f32];
        let steps = view.rig_from_view.len().min(RS_STEPS_MAX);
        u.misc = [steps as f32, self.config.format.is_srgb() as u8 as f32, 0.0, 0.0];
        u.lenses = self.lens_u;
        for (k, m) in view.rig_from_view.iter().take(steps).enumerate() {
            u.views[k * 3..k * 3 + 3].copy_from_slice(&rows(*m));
        }
        self.queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            _ => return Ok(()),
        };
        let target = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
        Ok(())
    }
}

fn lens_uniform(l: &Lens) -> LensU {
    let f = |v: f64| v as f32;
    LensU {
        r: rows(l.cam_from_rig),
        k: [f(l.k[0]), f(l.k[1]), f(l.k[2]), f(l.k[3])],
        k5p: [f(l.k[4]), f(l.p[0]), f(l.p[1]), f(l.skew)],
        fc: [f(l.focal[0]), f(l.focal[1]), f(l.centre[0]), f(l.centre[1])],
        a0: [0.0; 4],
        a1: [0.0; 4],
        lim: [f(l.fov_limit), f(l.blend.limit), f(l.blend.width), 0.0],
        calib: [l.calib_dim[0] as f32, l.calib_dim[1] as f32, 0.0, 0.0],
    }
}

fn rows(m: M3) -> [[f32; 4]; 3] {
    m.map(|r| [r[0] as f32, r[1] as f32, r[2] as f32, 0.0])
}

fn plane(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> Plane {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Plane { texture, view }
}

fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], w: u32, h: u32, bpp: u32) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * bpp), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
}

fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    slots: &[(Plane, Plane); 2],
    masks: &[Plane; 2],
) -> wgpu::BindGroup {
    fn tex(binding: u32, p: &Plane) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry { binding, resource: wgpu::BindingResource::TextureView(&p.view) }
    }
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            tex(2, &slots[0].0),
            tex(3, &slots[0].1),
            tex(4, &slots[1].0),
            tex(5, &slots[1].1),
            tex(6, &masks[0]),
            tex(7, &masks[1]),
        ],
    })
}
