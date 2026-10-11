//! The 360 view as an iced shader primitive: one pass of `shader.wgsl` over
//! the widget's rectangle, sharing iced's wgpu device. `Pipeline` doesn't
//! need iced: `render.rs` drives it on a device of its own for export.

use super::source::{Frame, Layout};
use crate::align::{Aligner, CELLS_ALPHA, CELLS_PHI, Field, Strips};
use crate::exposure::{self, BINS, FADE_FROM, FADE_TO, Gains, Seam};
use crate::stitch::{Lens, M3};
use crate::view::Camera;
use bytemuck::{Pod, Zeroable};
use iced::wgpu;
use iced::widget::shader::{self, Viewport};
use iced::Rectangle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Exposure seam basis rows; .w of the first two are the fade angles.
    seam: [[f32; 4]; 3],
    gains: [[f32; 4]; BINS * 2],
    /// Seam alignment shifts, four cells per vector.
    field: [[f32; 4]; CELLS_PHI * CELLS_ALPHA / 4],
    /// Disagreement cut strength per azimuth bin, four per vector.
    cut: [[f32; 4]; CELLS_PHI / 4],
}

/// Numbers rigs and frames so each is uploaded once however often it is drawn.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A clip's lens geometry; the pipeline rebuilds its masks only when another
/// clip is opened.
pub struct Rig {
    id: u64,
    pub lenses: Vec<Lens>,
    pub tracks: Vec<u32>,
    pub seam: Option<Seam>,
    pub aligner: Option<Aligner>,
}

impl Rig {
    pub fn new(lenses: Vec<Lens>, tracks: Vec<u32>) -> Self {
        let (seam, aligner) = (Seam::new(&lenses), Aligner::new(&lenses));
        Rig { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), lenses, tracks, seam, aligner }
    }
}

/// Per-frame seam corrections: exposure matching and close-subject alignment.
#[derive(Clone, Default)]
pub struct SeamFix {
    pub gains: Gains,
    pub field: Field,
}

/// What `SeamFix::follow` reads off a frame before matching it against the
/// last: split out so export can sample one frame while matching the one
/// before (14 and 17 ms a frame at 3840²).
pub struct SeamSample {
    gains: Option<Gains>,
    strips: Option<Strips>,
}

impl SeamSample {
    pub fn new(rig: &Rig, frame: &Frame) -> Self {
        let rgb = |i: usize, u: f64, v: f64| frame.rgb_at(&rig.lenses[i], rig.tracks[rig.lenses[i].input], u, v);
        SeamSample {
            gains: rig.seam.as_ref().map(|seam| seam.measure(rgb)),
            strips: rig.aligner.as_ref().map(|aligner| aligner.strips(|i, u, v| rgb(i, u, v).map(|c| c.iter().sum::<f64>() / 3.0))),
        }
    }
}

impl SeamFix {
    /// Measures `frame`'s seam. When it follows on from the last frame
    /// (`continuous`) the corrections ease towards it, else they jump.
    pub fn follow(&mut self, rig: &Rig, frame: &Frame, continuous: bool) {
        self.apply(rig, SeamSample::new(rig, frame), continuous);
    }

    pub fn apply(&mut self, rig: &Rig, sample: SeamSample, continuous: bool) {
        if let Some(now) = sample.gains {
            if continuous {
                self.gains.ease(&now, exposure::EASE);
            } else {
                self.gains = now;
            }
        }
        if let (Some(aligner), Some(strips)) = (&rig.aligner, sample.strips) {
            let now = aligner.measure(&strips, continuous.then_some(&self.field));
            if continuous {
                self.field.ease(&now, crate::align::EASE);
            } else {
                self.field = now;
            }
        }
    }
}

/// A decoded frame as handed to the view, with its seam corrections.
pub struct Shown {
    id: u64,
    frame: Frame,
    fix: SeamFix,
}

impl Shown {
    pub fn new(frame: Frame, fix: SeamFix) -> Self {
        Shown { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), frame, fix }
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }
}

/// Everything one draw needs; rebuilt by the view on every redraw.
pub struct Primitive {
    pub rig: Arc<Rig>,
    pub shown: Arc<Shown>,
    /// The user's free look.
    pub camera: Camera,
    /// rig_from_view at instants spread over the readout.
    pub rig_from_view: Vec<M3>,
}

impl std::fmt::Debug for Primitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Primitive").field("rig", &self.rig.id).finish_non_exhaustive()
    }
}

struct Plane {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    srgb: bool,
    /// Per texture slot: Y plane, UV plane.
    slots: [(Plane, Plane); 2],
    masks: [Plane; 2],
    bind_group: wgpu::BindGroup,
    lens_u: [LensU; 2],
    rig_id: u64,
    shown_id: u64,
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        Pipeline::create(device, format)
    }
}

impl Pipeline {
    pub fn create(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
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
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
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
                targets: &[Some(format.into())],
            }),
            multiview: None,
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
        let slots = [0, 1].map(|_| (plane(device, 2, 2, wgpu::TextureFormat::R8Unorm), plane(device, 1, 1, wgpu::TextureFormat::Rg8Unorm)));
        let masks = [0, 1].map(|_| plane(device, 1, 1, wgpu::TextureFormat::R8Unorm));
        let bind_group = bind(device, &layout, &uniforms, &sampler, &slots, &masks);
        Pipeline {
            pipeline,
            layout,
            sampler,
            uniforms,
            srgb: format.is_srgb(),
            slots,
            masks,
            bind_group,
            lens_u: [LensU::zeroed(); 2],
            rig_id: 0,
            shown_id: 0,
        }
    }

    /// Gets ready to draw `shown` through `camera` into a `size` pixel
    /// rectangle at `origin` in its target. The rig and pictures are
    /// uploaded only when they change.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_frame(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rig: &Rig, shown: &Shown, camera: &Camera, rig_from_view: &[M3], size: [f32; 2], origin: [f32; 2]) {
        if self.rig_id != rig.id {
            self.set_rig(device, queue, rig);
        }
        if self.shown_id != shown.id {
            self.upload(device, queue, &shown.frame, rig);
            self.shown_id = shown.id;
        }
        let mut u = Uniforms::zeroed();
        u.user = rows(camera.user);
        // A negative mapping means equirect; see the shader's `screen_ray`.
        u.screen = [size[0], size[1], camera.zoom as f32, camera.planet.map_or(-1.0, |d| d as f32)];
        let steps = rig_from_view.len().min(RS_STEPS_MAX);
        u.misc = [steps as f32, self.srgb as u8 as f32, origin[0], origin[1]];
        u.lenses = self.lens_u;
        for (k, m) in rig_from_view.iter().take(steps).enumerate() {
            u.views[k * 3..k * 3 + 3].copy_from_slice(&rows(*m));
        }
        if let Some(seam) = &rig.seam {
            u.seam = rows(seam.ring.basis);
            u.seam[0][3] = FADE_FROM as f32;
            u.seam[1][3] = FADE_TO as f32;
        }
        for (lens, bins) in shown.fix.gains.0.iter().enumerate() {
            for (b, g) in bins.iter().enumerate() {
                u.gains[lens * BINS + b] = [g[0], g[1], g[2], 0.0];
            }
        }
        for (v, cells) in u.field.iter_mut().zip(shown.fix.field.shift.chunks_exact(4)) {
            v.copy_from_slice(cells);
        }
        for (v, bins) in u.cut.iter_mut().zip(shown.fix.field.cut.chunks_exact(4)) {
            v.copy_from_slice(bins);
        }
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
    }

    /// Draws the prepared frame into `target`, within `clip`.
    pub fn draw(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, clip: &Rectangle<u32>) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("stitch"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_scissor_rect(clip.x, clip.y, clip.width, clip.height);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn set_rig(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rig: &Rig) {
        self.masks = [0, 1].map(|i| {
            let l = &rig.lenses[i];
            let [w, h] = l.calib_dim;
            let p = plane(device, w as u32, h as u32, wgpu::TextureFormat::R8Unorm);
            write(queue, &p.texture, &l.edge, w as u32, h as u32, 1);
            p
        });
        self.lens_u = [0, 1].map(|i| lens_uniform(&rig.lenses[i]));
        self.bind_group = bind(device, &self.layout, &self.uniforms, &self.sampler, &self.slots, &self.masks);
        self.rig_id = rig.id;
        self.shown_id = 0;
    }

    /// Uploads a frame's pictures and points each lens at its part of them.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &Frame, rig: &Rig) {
        let mut rebind = false;
        for (slot, pic) in frame.pictures.iter().enumerate().take(2) {
            let (y, uv) = &mut self.slots[slot];
            if y.texture.width() != pic.width || y.texture.height() != pic.height {
                *y = plane(device, pic.width, pic.height, wgpu::TextureFormat::R8Unorm);
                *uv = plane(device, pic.width / 2, pic.height / 2, wgpu::TextureFormat::Rg8Unorm);
                rebind = true;
            }
            write(queue, &y.texture, pic.y(), pic.width, pic.height, 1);
            write(queue, &uv.texture, pic.uv(), pic.width / 2, pic.height / 2, 2);
        }
        if rebind {
            self.bind_group = bind(device, &self.layout, &self.uniforms, &self.sampler, &self.slots, &self.masks);
        }
        for (i, lens) in rig.lenses.iter().enumerate().take(2) {
            let track = rig.tracks[lens.input];
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
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(&self, p: &mut Pipeline, device: &wgpu::Device, queue: &wgpu::Queue, bounds: &Rectangle, viewport: &Viewport) {
        let px = *bounds * viewport.scale_factor();
        p.prepare_frame(device, queue, &self.rig, &self.shown, &self.camera, &self.rig_from_view, [px.width, px.height], [px.x, px.y]);
    }

    fn render(&self, p: &Pipeline, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, clip: &Rectangle<u32>) {
        p.draw(encoder, target, clip);
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
