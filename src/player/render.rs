//! Export on the GPU: the player's stitch shader (`gpu.rs`) rendering into
//! an off-screen texture on a wgpu device of its own, read back for the
//! encoder. The CPU stitcher in `stitch.rs` stays the reference (`export
//! --cpu`), and the two must match.
//!
//! The work overlaps in stages, each on its own thread: ffmpeg decodes, one
//! thread samples each frame's seam, the next matches it against the frame
//! before and works out the attitude, and the GPU renders a frame while the
//! last one is read back and encoded. Done one after another on 0037 at
//! 1080p the steps took 22 ms (seam), 14 ms (upload), 10 ms (GPU) and 6 ms
//! (conversion) a frame, and the export ran at 11 fps; overlapped, with
//! CUDA's NV12 decode, 21 fps.

use super::gpu::{self, Rig, SeamFix, SeamSample, Shown};
use super::source::{self, Frame, Kind};
use crate::ffmpeg::{self, Hwaccel};
use crate::stitch::{self, M3};
use crate::view::Camera;
use anyhow::{Context, Result, bail};
use iced::Rectangle;
use iced::wgpu;
use rayon::prelude::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel, sync_channel};

/// Display-encoded RGBA, as the shader writes it for a non-sRGB target.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Frames in flight on the GPU: one renders while the other is read back.
const IN_FLIGHT: usize = 2;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    stitcher: gpu::Pipeline,
    size: (usize, usize),
    targets: Vec<Target>,
    /// Submitted frames, oldest first: the target each renders into, and
    /// word of its readback being mapped.
    pending: VecDeque<(usize, Receiver<Result<(), wgpu::BufferAsyncError>>)>,
    next: usize,
    /// The adapter's name, for the log.
    pub name: String,
}

/// What a frame renders into, and the buffer it is read back through.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    /// Bytes per row in `readback`, padded as copies require.
    row: usize,
}

impl Gpu {
    /// A device of our own on the fastest GPU. A software rasteriser
    /// doesn't count: the CPU stitcher is quicker than that.
    pub fn new() -> Result<Gpu> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let options = wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() };
        let adapter = block_on(instance.request_adapter(&options)).context("no GPU adapter")?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            bail!("only a software renderer ({})", info.name);
        }
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("export"), ..Default::default() }))?;
        let stitcher = gpu::Pipeline::create(&device, FORMAT);
        Ok(Gpu { device, queue, stitcher, size: (0, 0), targets: Vec::new(), pending: VecDeque::new(), next: 0, name: info.name })
    }

    /// Starts rendering `shown` through `camera` at `size`; `finish` hands
    /// the frames back in order. At most `IN_FLIGHT` may be waiting, so
    /// `finish` one first when that many are.
    pub fn submit(&mut self, rig: &Rig, shown: &Shown, camera: &Camera, views: &[M3], (w, h): (usize, usize)) {
        assert!(self.pending.len() < IN_FLIGHT, "finish a frame before submitting another");
        if self.size != (w, h) {
            assert!(self.pending.is_empty(), "the size changed with frames in flight");
            self.targets = (0..IN_FLIGHT).map(|_| self.make_target(w, h)).collect();
            self.size = (w, h);
        }
        let slot = self.next;
        self.next = (self.next + 1) % IN_FLIGHT;
        let target = &self.targets[slot];
        self.stitcher.prepare_frame(&self.device, &self.queue, rig, shown, camera, views, [w as f32, h as f32], [0.0, 0.0]);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.stitcher.draw(&mut encoder, &target.view, &Rectangle { x: 0, y: 0, width: w as u32, height: h as u32 });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &target.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(target.row as u32), rows_per_image: Some(h as u32) },
            },
            wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = channel();
        target.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.pending.push_back((slot, rx));
    }

    /// The oldest submitted frame as RGB, waiting for the GPU if need be;
    /// `None` if none is in flight.
    pub fn finish(&mut self) -> Result<Option<stitch::Frame>> {
        let Some((slot, mapped)) = self.pending.pop_front() else { return Ok(None) };
        let (w, h) = self.size;
        let target = &self.targets[slot];
        // Callbacks fire only from a poll; wait until this one has.
        let result = loop {
            match mapped.try_recv() {
                Ok(r) => break r,
                Err(_) => {
                    self.device.poll(wgpu::PollType::wait_indefinitely())?;
                }
            }
        };
        result.context("reading the frame back from the GPU")?;
        let data = target.readback.slice(..).get_mapped_range();
        let mut rgb = vec![0; w * h * 3];
        rgb.par_chunks_mut(w * 3).zip(data.par_chunks(target.row)).for_each(|(out, row)| {
            for (o, px) in out.chunks_exact_mut(3).zip(row.chunks_exact(4)) {
                o.copy_from_slice(&px[..3]);
            }
        });
        drop(data);
        target.readback.unmap();
        Ok(Some(stitch::Frame { width: w, height: h, rgb }))
    }

    fn make_target(&self, w: usize, h: usize) -> Target {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("export"),
            size: wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let row = (w * 4).div_ceil(align) * align;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Target { texture, view, readback, row }
    }
}

/// What the GPU needs of a clip besides `crate::Pipeline`.
pub struct Clip {
    /// The GPU's copy of the rig, without the seam corrections the
    /// pipeline was set up without.
    rig: Arc<Rig>,
    /// What decodes the lens tracks: CUDA if it can, which converts to NV12
    /// on the GPU (see `ffmpeg::nv12`), else the pipeline's decoder.
    hw: Hwaccel,
}

impl Clip {
    pub fn new(p: &crate::Pipeline) -> Result<Clip> {
        let mut rig = Rig::new(p.lenses.clone(), p.tracks.clone(), p.seam.is_some());
        if p.aligner.is_none() {
            rig.aligner = None;
        }
        let cuda = p.proxy.is_none() && p.hw != Hwaccel::Software && p.hw != Hwaccel::Cuda && Hwaccel::Cuda.works(&p.input, p.tracks[0])?;
        Ok(Clip { rig: Arc::new(rig), hw: if cuda { Hwaccel::Cuda } else { p.hw } })
    }
}

/// A decoded frame with what the shader needs besides the picture.
struct Measured {
    frame: Frame,
    fix: SeamFix,
    views: Vec<M3>,
    t: f64,
}

/// Stitches `total` frames of `p`'s clip from `start` into `encoder` on the
/// GPU, as `crate::encode` does on the CPU: the same frame times, attitude
/// and rolling-shutter steps, and the last frame repeated if the clip ends
/// early so the sound stays in step.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    gpu: &mut Gpu,
    p: &crate::Pipeline,
    clip: &Clip,
    start: f64,
    total: usize,
    encoder: &mut dyn ffmpeg::FrameSink,
    camera: impl Fn(f64) -> Camera,
    size: (usize, usize),
    tick: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<()> {
    let kind = match &p.proxy {
        Some(proxy) => Kind::Lrf(proxy.clone()),
        None => Kind::Osv { path: p.input.clone(), tracks: p.tracks.clone(), size: None },
    };
    let stream = source::spawn(kind, clip.hw, start, p.fps)?;
    let rig = &clip.rig;
    // Seam and attitude for the next frames while this one renders. The
    // threads end with the stream, or when this returns and drops `measured`.
    let (sampled_tx, sampled) = sync_channel::<(Frame, SeamSample)>(2);
    let sample_rig = rig.clone();
    std::thread::spawn(move || {
        for frame in stream.frames.iter().take(total) {
            let sample = SeamSample::new(&sample_rig, &frame);
            if sampled_tx.send((frame, sample)).is_err() {
                break;
            }
        }
    });
    let (tx, measured) = sync_channel::<Measured>(2);
    let (seam_rig, stab, readout_ms, rs_steps, cut, fps) = (rig.clone(), p.stab.clone(), p.readout_ms, p.rs_steps, p.cut, p.fps);
    std::thread::spawn(move || {
        let mut fix = SeamFix::default();
        for (n, (frame, sample)) in sampled.iter().enumerate() {
            let t = start + n as f64 / fps;
            fix.apply(&seam_rig, sample, n > 0);
            if !cut {
                fix.field.cut.fill(0.0);
            }
            let views = if stab.has_data() { stab.rig_from_view(t * 1000.0, readout_ms, rs_steps) } else { vec![stitch::IDENTITY] };
            if tx.send(Measured { frame, fix: fix.clone(), views, t }).is_err() {
                break;
            }
        }
    });
    let mut last: Option<stitch::Frame> = None;
    let mut done = 0;
    let mut write = |frame: &stitch::Frame| -> Result<usize> {
        encoder.write(frame)?;
        done += 1;
        tick(done)?;
        Ok(done)
    };
    let mut written = 0;
    for m in measured.iter() {
        if gpu.pending.len() == IN_FLIGHT
            && let Some(frame) = gpu.finish()?
        {
            written = write(&frame)?;
            last = Some(frame);
        }
        gpu.submit(rig, &Shown::new(m.frame, m.fix), &camera(m.t), &m.views, size);
    }
    while let Some(frame) = gpu.finish()? {
        written = write(&frame)?;
        last = Some(frame);
    }
    // The clip ran out before the cut did.
    let last = last.context("no frames decoded")?;
    for _ in written..total {
        write(&last)?;
    }
    Ok(())
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    iced::futures::executor::block_on(f)
}
