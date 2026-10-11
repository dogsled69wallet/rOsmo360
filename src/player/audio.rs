//! Sound for the player: the clip's audio decoded by ffmpeg to float PCM at
//! the output device's own rate and channel count, played through cpal.
//!
//! While audio plays it is the playback clock: the video shows whatever
//! frame matches the samples actually heard, so the two cannot drift apart
//! however long the clip. If decoding falls behind, the device gets silence
//! and the clock stops with it.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How much decoded audio to keep ahead of the device.
const BUFFER_SECONDS: f64 = 0.5;

struct Shared {
    samples: Mutex<VecDeque<f32>>,
    /// Frames handed to the device so far.
    played: AtomicU64,
    /// The device's latency from callback to speaker, in microseconds.
    latency_us: AtomicU64,
    /// f32 bits.
    volume: AtomicU32,
    /// Until `play`, the device gets silence and the buffer is left alone.
    /// Pausing the stream instead isn't enough: not every backend can (on
    /// PipeWire's ALSA plugin a prepared cut's sound played under the cut
    /// before it, its buffer drained, so its clock ran early).
    running: AtomicBool,
    stop: AtomicBool,
}

pub struct Audio {
    _stream: cpal::Stream,
    child: Child,
    shared: Arc<Shared>,
    rate: f64,
}

impl Audio {
    /// Starts playing `input`'s first audio track from `start` seconds.
    pub fn start(input: &Path, start: f64, volume: f32) -> Result<Audio> {
        let audio = Audio::prepare(input, start, volume)?;
        audio.play();
        Ok(audio)
    }

    /// Opens the device and starts decoding from `start`, filling the buffer,
    /// but plays silence until `play`: the next cut's sound, ready ahead, so
    /// moving on to it needn't wait for the device or ffmpeg.
    pub fn prepare(input: &Path, start: f64, volume: f32) -> Result<Audio> {
        let device = cpal::default_host().default_output_device().context("no audio output device")?;
        let supported = device.default_output_config().context("querying the audio device")?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let channels = config.channels as usize;
        let rate = config.sample_rate as f64;

        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-ss", &start.to_string(), "-i"])
            .arg(input)
            .args(["-map", "0:a:0", "-f", "f32le", "-ac", &channels.to_string(), "-ar", &config.sample_rate.to_string(), "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running ffmpeg")?;
        let shared = Arc::new(Shared {
            samples: Mutex::new(VecDeque::new()),
            played: AtomicU64::new(0),
            latency_us: AtomicU64::new(0),
            volume: AtomicU32::new(volume.to_bits()),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });

        let mut out = child.stdout.take().unwrap();
        let reader = shared.clone();
        let ahead = (BUFFER_SECONDS * rate) as usize * channels;
        thread::spawn(move || {
            let mut bytes = vec![0u8; 4096 * 4];
            while !reader.stop.load(Ordering::Relaxed) {
                if reader.samples.lock().unwrap().len() > ahead {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let Ok(n) = out.read(&mut bytes) else { break };
                if n == 0 {
                    break;
                }
                // A read can end mid-sample; keep only whole ones.
                let whole = n / 4 * 4;
                let mut q = reader.samples.lock().unwrap();
                q.extend(bytes[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                drop(q);
                if whole < n {
                    let rest = n - whole;
                    let mut tail = vec![0u8; 4 - rest];
                    if out.read_exact(&mut tail).is_err() {
                        break;
                    }
                    let mut b = [0u8; 4];
                    b[..rest].copy_from_slice(&bytes[whole..n]);
                    b[rest..].copy_from_slice(&tail);
                    reader.samples.lock().unwrap().push_back(f32::from_le_bytes(b));
                }
            }
        });

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, shared.clone()),
            SampleFormat::I16 => build::<i16>(&device, &config, shared.clone()),
            SampleFormat::I32 => build::<i32>(&device, &config, shared.clone()),
            SampleFormat::U16 => build::<u16>(&device, &config, shared.clone()),
            other => bail!("unsupported audio sample format {other}"),
        }?;
        stream.play().context("starting audio")?;
        Ok(Audio { _stream: stream, child, shared, rate })
    }

    /// Lets the sound through: from the next device callback, milliseconds.
    pub fn play(&self) {
        self.shared.running.store(true, Ordering::Relaxed);
    }

    /// Seconds of audio heard since `start`.
    pub fn elapsed(&self) -> f64 {
        let played = self.shared.played.load(Ordering::Relaxed) as f64 / self.rate;
        (played - self.shared.latency_us.load(Ordering::Relaxed) as f64 / 1e6).max(0.0)
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.to_bits(), Ordering::Relaxed);
    }

    /// Seconds from handing samples to the device to hearing them, as the
    /// device last reported (0 before its first callback).
    pub fn latency(&self) -> f64 {
        self.shared.latency_us.load(Ordering::Relaxed) as f64 / 1e6
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn build<T: SizedSample + FromSample<f32>>(device: &cpal::Device, config: &StreamConfig, shared: Arc<Shared>) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], info: &OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback);
            shared.latency_us.store(latency.as_micros() as u64, Ordering::Relaxed);
            if !shared.running.load(Ordering::Relaxed) {
                data.fill(T::EQUILIBRIUM);
                return;
            }
            let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            let mut q = shared.samples.lock().unwrap();
            // Only whole frames count towards the clock; an underrun plays
            // silence without advancing it.
            let frames = (q.len() / channels).min(data.len() / channels);
            for (i, s) in data.iter_mut().enumerate() {
                *s = T::from_sample(if i < frames * channels { q.pop_front().unwrap_or(0.0) * volume } else { 0.0 });
            }
            drop(q);
            shared.played.fetch_add(frames as u64, Ordering::Relaxed);
        },
        |e| eprintln!("audio: {e}"),
        None,
    )?;
    Ok(stream)
}
