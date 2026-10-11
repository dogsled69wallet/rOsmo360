//! An edit: cuts from one or more clips played one after another, each with
//! its own keyframed views. The player's timeline edits it and `export`
//! renders it.
//!
//! Keyframes belong to their cut and are pinned to clip time, not to the
//! timeline: moving a cut keeps its camera moves, and trimming one keeps
//! each keyframe on the same moment of footage. Keyframes outside the cut's
//! in and out points are kept, so extending the cut again brings them back.
//!
//! Music is the other way round: its clips sit at times on the edit, so
//! cutting the video to the music leaves the music where it is. It is cut
//! on its own lane, and the edit ends with the last video cut, the music
//! faded out there if it runs on.

use crate::stab::Mode;
use crate::view::{Aim, Aspect, Camera, Keyframes, Projection, View};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The picture's shape, width:height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect: Option<Aspect>,
    /// None yet when a project starts from its music.
    #[serde(default)]
    pub cuts: Vec<Cut>,
    /// The clips' own sound in the mix.
    #[serde(default, skip_serializing_if = "Sound::is_default")]
    pub sound: Sound,
    #[serde(default, skip_serializing_if = "Music::is_empty")]
    pub music: Music,
}

/// The clips' own sound, as one track of the mix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sound {
    /// Level in dB; 0 is as recorded.
    #[serde(default)]
    pub volume: f64,
}

impl Sound {
    fn is_default(&self) -> bool {
        *self == Sound::default()
    }
}

/// The music track: clips of songs placed on the edit's timeline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Music {
    /// The track's level in dB; 0 is as the files are.
    #[serde(default)]
    pub volume: f64,
    #[serde(default)]
    pub clips: Vec<MusicClip>,
}

impl Music {
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty() && self.volume == 0.0
    }

    /// Where the last clip ends on the edit, in seconds.
    pub fn end(&self) -> f64 {
        self.clips.iter().map(|c| c.at + c.end - c.start).fold(0.0, f64::max)
    }
}

/// A stretch of a song at a place on the edit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MusicClip {
    /// The file; a relative path is relative to the project file.
    pub input: PathBuf,
    /// Where it starts on the edit, in seconds.
    pub at: f64,
    /// In and out points in the song, in seconds.
    #[serde(rename = "in")]
    pub start: f64,
    #[serde(rename = "out")]
    pub end: f64,
    /// Fades at its own start and end, in seconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_in: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_out: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Music running on past the last cut fades out over this long before it.
pub const END_FADE: f64 = 3.0;

/// A level at or below this is off: the sliders' bottom is silence.
pub const OFF_DB: f64 = -40.0;

/// A mix level in dB as a gain factor.
pub fn gain(db: f64) -> f64 {
    if db <= OFF_DB { 0.0 } else { 10f64.powf(db / 20.0) }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    /// The clip; a relative path is relative to the project file.
    pub input: PathBuf,
    /// In and out points, clip time in seconds.
    #[serde(rename = "in")]
    pub start: f64,
    #[serde(rename = "out")]
    pub end: f64,
    #[serde(default)]
    pub keyframes: Keyframes,
    /// Its stabilisation, or the export's `--stabilize` (horizon by
    /// default) if it doesn't set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stabilize: Option<Mode>,
    /// What the cut is for, shown on the timeline: written by whoever
    /// assembled the edit, such as an automatic highlight pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Project {
    pub fn load(path: &Path) -> Result<Project> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut project: Project = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        // Music alone is a start: its length is the frame to fill.
        if project.cuts.is_empty() && project.music.clips.is_empty() {
            bail!("{} has no cuts", path.display());
        }
        let dir = path.parent().unwrap_or(Path::new(""));
        for (n, cut) in project.cuts.iter_mut().enumerate() {
            if cut.end <= cut.start {
                bail!("cut {} ends ({} s) before it starts ({} s)", n + 1, cut.end, cut.start);
            }
            cut.input = dir.join(&cut.input);
            for track in cut.keyframes.tracks_mut() {
                *track = dir.join(&*track);
            }
        }
        for (n, clip) in project.music.clips.iter_mut().enumerate() {
            if clip.end <= clip.start || clip.at < 0.0 {
                bail!("music clip {} is empty or starts before the edit", n + 1);
            }
            clip.input = dir.join(&clip.input);
        }
        Ok(project)
    }

    /// Writes the project as JSON. Clips in the project's folder or below
    /// are stored relative to it, so the folder can move as a whole.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new(""));
        let mut project = self.clone();
        for cut in &mut project.cuts {
            if let Ok(rel) = cut.input.strip_prefix(dir) {
                cut.input = rel.to_owned();
            }
            for track in cut.keyframes.tracks_mut() {
                if let Ok(rel) = track.strip_prefix(dir) {
                    *track = rel.to_owned();
                }
            }
        }
        for clip in &mut project.music.clips {
            if let Ok(rel) = clip.input.strip_prefix(dir) {
                clip.input = rel.to_owned();
            }
        }
        std::fs::write(path, serde_json::to_string_pretty(&project)? + "\n").with_context(|| format!("writing {}", path.display()))
    }

    /// Length of the edit in seconds.
    pub fn duration(&self) -> f64 {
        self.cuts.iter().map(|c| c.end - c.start).sum()
    }
}

/// The keyframes that frame cut `n` of `cuts` (in edit order) at clip time
/// `t`, and the time to read them at. A cut without keyframes holds the view
/// the edit had last: the end of the nearest earlier cut with some, as the
/// player shows it when playing on. `None` before any keyframe.
pub fn keyframed(cuts: &[Cut], n: usize, t: f64) -> Option<(&Keyframes, f64)> {
    if !cuts[n].keyframes.is_empty() {
        return Some((&cuts[n].keyframes, t));
    }
    cuts[..n].iter().rev().find(|c| !c.keyframes.is_empty()).map(|c| (&c.keyframes, c.end))
}

/// The view of cut `n` at clip time `t` (see `keyframed`), or straight ahead
/// before any keyframe. `aim` is where following keyframes point in this
/// cut's clip at `t`, so a cut that holds an earlier cut's following view
/// goes on following in its own footage.
pub fn camera_at(cuts: &[Cut], n: usize, t: f64, aim: Aim) -> Camera {
    keyframed(cuts, n, t).and_then(|(k, at)| k.camera_at(at, &held(aim, at == t, t))).unwrap_or_else(|| View::home(Projection::Perspective).camera())
}

/// `aim` for reading keyframes from `keyframed`: as asked in the cut's own
/// keyframes, else always at the cut's time `t`, as the keyframes held
/// from an earlier cut are read at that cut's end.
pub fn held(aim: Aim<'_>, own: bool, t: f64) -> impl Fn(&crate::view::Follow, f64) -> Option<(f64, f64)> + '_ {
    move |f, at| aim(f, if own { at } else { t })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_cuts_relative_to_the_file() {
        let dir = std::env::temp_dir().join(format!("rosmo360-project-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edit.json");
        std::fs::write(
            &path,
            r#"{"aspect": "9:16", "cuts": [
                {"input": "a.OSV", "in": 28, "out": 37, "keyframes": [{"t": 28, "yaw": -20}, {"t": 37, "projection": "little-planet"}, {"t": 37.5, "follow": "friend.csv"}]},
                {"input": "/clips/b.OSV", "in": 5, "out": 12, "stabilize": "lock", "note": "the pan"}
            ]}"#,
        )
        .unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!(p.cuts[0].input, dir.join("a.OSV"));
        assert_eq!(p.cuts[1].input, PathBuf::from("/clips/b.OSV"));
        assert_eq!(p.duration(), 16.0);
        assert_eq!(p.cuts[1].keyframes.len(), 0);
        assert_eq!((p.cuts[0].stabilize, p.cuts[1].stabilize), (None, Some(Mode::Lock)));
        assert_eq!((p.cuts[0].note.as_deref(), p.cuts[1].note.as_deref()), (None, Some("the pan")));
        assert_eq!(p.cuts[0].keyframes[1].view.projection, Projection::LittlePlanet);
        assert_eq!(p.cuts[0].keyframes[2].follow, Some(crate::view::Follow::Track(dir.join("friend.csv"))));
        let saved = dir.join("again.json");
        p.save(&saved).unwrap();
        let text = std::fs::read_to_string(&saved).unwrap();
        assert!(text.contains(r#""input": "a.OSV""#) && text.contains(r#""follow": "friend.csv""#));
        assert_eq!(Project::load(&saved).unwrap(), p);
        std::fs::write(&path, r#"{"cuts": [{"input": "a.OSV", "in": 5, "out": 2}]}"#).unwrap();
        assert!(Project::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn music_sits_on_the_edit_relative_to_the_file() {
        let dir = std::env::temp_dir().join(format!("rosmo360-music-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edit.json");
        // Music alone is a project: the song is the frame to fill.
        std::fs::write(&path, r#"{"music": {"volume": -3, "clips": [{"input": "song.mp3", "at": 2, "in": 60, "out": 200, "fade_in": 1.5}]}}"#).unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!((p.cuts.len(), p.sound.volume, p.music.volume), (0, 0.0, -3.0));
        assert_eq!(p.music.clips[0].input, dir.join("song.mp3"));
        assert_eq!(p.music.end(), 142.0);
        p.save(&dir.join("again.json")).unwrap();
        let text = std::fs::read_to_string(dir.join("again.json")).unwrap();
        assert!(text.contains(r#""input": "song.mp3""#) && !text.contains("fade_out") && !text.contains("sound"));
        assert_eq!(Project::load(&dir.join("again.json")).unwrap(), p);
        // Without music or sound settings nothing is written for them.
        assert!(!serde_json::to_string(&Project::default()).unwrap().contains("music"));
        std::fs::write(&path, r#"{"music": {"clips": [{"input": "song.mp3", "at": 0, "in": 9, "out": 9}]}}"#).unwrap();
        assert!(Project::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cuts_without_keyframes_hold_the_last_view() {
        let p: Project = serde_json::from_str(
            r#"{"cuts": [
                {"input": "a.OSV", "in": 0, "out": 4},
                {"input": "a.OSV", "in": 10, "out": 20, "keyframes": [{"t": 10, "yaw": 0}, {"t": 14, "yaw": 40}, {"t": 30, "yaw": 200}]},
                {"input": "b.OSV", "in": 50, "out": 60},
                {"input": "b.OSV", "in": 70, "out": 80}
            ]}"#,
        )
        .unwrap();
        let yaw = |n, t| camera_at(&p.cuts, n, t, &|_, _| None).user;
        assert_eq!(yaw(0, 2.0), View::home(Projection::Perspective).camera().user);
        assert_eq!(keyframed(&p.cuts, 1, 12.0).map(|k| k.1), Some(12.0));
        // The view at the earlier cut's out point, not at its last keyframe.
        let held = p.cuts[1].keyframes.camera_at(20.0, &|_, _| None).unwrap().user;
        assert_eq!((yaw(2, 55.0), yaw(3, 75.0)), (held, held));
    }
}
