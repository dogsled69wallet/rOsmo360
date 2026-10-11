//! The music track in the player: each song's waveform for the timeline,
//! and its sound kept in step with the edit while it plays.
//!
//! Music has an audio stream of its own, apart from the clips' sound, so it
//! plays on through cut changes untouched. It follows edit time: started at
//! the song's place when playback reaches a music clip, restarted only after
//! a jump (or if it drifts), and set to its level every frame, which carries
//! the clip's fades and the fade-out where the video ends. The next clip is
//! opened `PRIME` seconds ahead and let through one device latency early, so
//! one song hands over to the next without the gap of starting ffmpeg and the
//! device; a clip that carries on in the same song just plays on.

use super::audio::Audio;
use crate::project::{self, MusicClip};
use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Seconds per waveform peak.
pub const WAVE_STEP: f64 = 0.05;
/// Restart the music when it is this far from where the edit is.
const DRIFT: f64 = 0.15;
/// Open the next clip's sound this long before it starts.
const PRIME: f64 = 1.0;

/// A song's waveform: the loudest sample in each `WAVE_STEP`, 0..1, and the
/// song's length.
#[derive(Debug)]
pub struct Waveform {
    pub peaks: Vec<f32>,
    pub duration: f64,
}

impl Waveform {
    /// Decodes `path` to mono at 8 kHz, which is plenty for a picture of its
    /// loudness: a 6-minute song takes about a second.
    pub fn read(path: &Path) -> Result<Waveform> {
        const RATE: usize = 8000;
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-ac", "1", "-ar", &RATE.to_string(), "-f", "f32le", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running ffmpeg")?;
        let mut bytes = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut bytes)?;
        child.wait()?;
        let per = (WAVE_STEP * RATE as f64) as usize;
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs()).collect();
        if samples.is_empty() {
            anyhow::bail!("{} has no sound", path.display());
        }
        let peaks = samples.chunks(per).map(|c| c.iter().fold(0.0f32, |m, &v| m.max(v)).min(1.0)).collect();
        Ok(Waveform { peaks, duration: samples.len() as f64 / RATE as f64 })
    }
}

/// A mix level in dB as the speakers' gain factor.
pub fn gain(db: f64) -> f32 {
    project::gain(db) as f32
}

/// How loud music clip `clip` is at edit time `t` from its own fades and the
/// fade-out before `video_end` when it runs on past it, 0..1.
fn envelope(clip: &MusicClip, t: f64, video_end: f64) -> f64 {
    let into = t - clip.at;
    let ends = clip.at + clip.end - clip.start;
    let mut g: f64 = 1.0;
    if clip.fade_in > 0.0 {
        g = g.min(into / clip.fade_in);
    }
    if clip.fade_out > 0.0 {
        g = g.min((ends - t) / clip.fade_out);
    }
    if ends > video_end {
        g = g.min((video_end - t) / project::END_FADE);
    }
    g.clamp(0.0, 1.0)
}

/// A clip's sound, open: its song, where the clip sits on the edit, and from
/// where in the song the stream started (so it's at `from + elapsed` now).
struct Open {
    input: PathBuf,
    at: f64,
    from: f64,
    audio: Audio,
}

impl Open {
    fn is(&self, clip: &MusicClip) -> bool {
        self.input == clip.input && self.at == clip.at && self.from == clip.start
    }
}

/// The music playing now, and the next clip's, ready.
#[derive(Default)]
pub struct Player {
    playing: Option<Open>,
    next: Option<Open>,
}

impl Player {
    /// Keeps the music in step with edit time `t` while the edit plays (stops
    /// it otherwise): the clip under `t`, at `listen` (the speakers' volume)
    /// times the track's level and the clip's envelope.
    pub fn follow(&mut self, track: &project::Music, t: f64, video_end: f64, listen: f32, playing: bool) {
        if !playing {
            (self.playing, self.next) = (None, None);
            return;
        }
        let level = |clip: &MusicClip, t: f64| listen * gain(track.volume) * envelope(clip, t, video_end) as f32;
        let clip = track.clips.iter().find(|c| t >= c.at && t < c.ends() && t < video_end);
        // Moving on to the clip that was made ready.
        if let Some(clip) = clip
            && self.next.as_ref().is_some_and(|n| n.is(clip))
        {
            let next = self.next.take().unwrap();
            next.audio.play();
            self.playing = Some(next);
        }
        match clip {
            Some(clip) => {
                let want = clip.start + (t - clip.at);
                let volume = level(clip, t);
                match &mut self.playing {
                    // Within the same song and in step: a split clip's second
                    // half just plays on.
                    Some(open) if open.input == clip.input && (open.from + open.audio.elapsed() - want).abs() < DRIFT => open.audio.set_volume(volume),
                    _ => {
                        self.playing = match Audio::start(&clip.input, want, volume) {
                            Ok(audio) => Some(Open { input: clip.input.clone(), at: clip.at, from: want, audio }),
                            Err(e) => {
                                eprintln!("no music: {e:#}");
                                None
                            }
                        }
                    }
                }
            }
            None => self.playing = None,
        }
        self.prime(track, clip, t, video_end, &level);
    }

    /// Opens the clip starting next within `PRIME` of `t`, unless the one
    /// playing runs straight on into it, and lets it through a device latency
    /// before it starts, so it's heard on time.
    fn prime(&mut self, track: &project::Music, now: Option<&MusicClip>, t: f64, video_end: f64, level: &dyn Fn(&MusicClip, f64) -> f32) {
        let next = track.clips.iter().filter(|c| c.at > t && c.at < t + PRIME && c.at < video_end).min_by(|a, b| a.at.total_cmp(&b.at));
        let carries_on = |n: &MusicClip| now.is_some_and(|c| c.input == n.input && (c.ends() - n.at).abs() < 1e-3 && (c.end - n.start).abs() < 1e-3);
        let Some(next) = next.filter(|n| !carries_on(n)) else {
            self.next = None;
            return;
        };
        if !self.next.as_ref().is_some_and(|n| n.is(next)) {
            self.next = match Audio::prepare(&next.input, next.start, level(next, next.at)) {
                Ok(audio) => Some(Open { input: next.input.clone(), at: next.at, from: next.start, audio }),
                Err(e) => {
                    eprintln!("no music: {e:#}");
                    None
                }
            };
        }
        if let Some(open) = &self.next
            && t >= next.at - open.audio.latency().min(0.25)
        {
            open.audio.play();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades_in_out_and_before_the_video_ends() {
        let clip = MusicClip { input: "song.mp3".into(), at: 10.0, start: 0.0, end: 100.0, fade_in: 2.0, fade_out: 0.0 };
        assert_eq!(envelope(&clip, 11.0, 1000.0), 0.5);
        assert_eq!(envelope(&clip, 50.0, 1000.0), 1.0);
        // The video ends at 60 s, so it fades over the last 3 s before it.
        assert_eq!(envelope(&clip, 58.5, 60.0), 0.5);
        assert_eq!(envelope(&clip, 56.0, 60.0), 1.0);
        let short = MusicClip { fade_out: 4.0, end: 20.0, ..clip };
        assert_eq!(envelope(&short, 28.0, 1000.0), 0.5);
    }
}
