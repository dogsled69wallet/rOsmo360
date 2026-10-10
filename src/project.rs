//! An edit: cuts from one or more clips played one after another, each with
//! its own keyframed views. The player's timeline edits it and `export`
//! renders it.
//!
//! Keyframes belong to their cut and are pinned to clip time, not to the
//! timeline: moving a cut keeps its camera moves, and trimming one keeps
//! each keyframe on the same moment of footage. Keyframes outside the cut's
//! in and out points are kept, so extending the cut again brings them back.

use crate::view::{Aspect, Camera, Keyframes, Projection, View};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The picture's shape, width:height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect: Option<Aspect>,
    pub cuts: Vec<Cut>,
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
}

impl Project {
    pub fn load(path: &Path) -> Result<Project> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut project: Project = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        if project.cuts.is_empty() {
            bail!("{} has no cuts", path.display());
        }
        let dir = path.parent().unwrap_or(Path::new(""));
        for (n, cut) in project.cuts.iter_mut().enumerate() {
            if cut.end <= cut.start {
                bail!("cut {} ends ({} s) before it starts ({} s)", n + 1, cut.end, cut.start);
            }
            cut.input = dir.join(&cut.input);
        }
        Ok(project)
    }

    /// Length of the edit in seconds.
    pub fn duration(&self) -> f64 {
        self.cuts.iter().map(|c| c.end - c.start).sum()
    }
}

impl Cut {
    /// The view at clip time `t`: the keyframes', or straight ahead without any.
    pub fn camera_at(&self, t: f64) -> Camera {
        self.keyframes.camera_at(t).unwrap_or_else(|| View::home(Projection::Perspective).camera())
    }
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
                {"input": "a.OSV", "in": 28, "out": 37, "keyframes": [{"t": 28, "yaw": -20}, {"t": 37, "projection": "little-planet"}]},
                {"input": "/clips/b.OSV", "in": 5, "out": 12}
            ]}"#,
        )
        .unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!(p.cuts[0].input, dir.join("a.OSV"));
        assert_eq!(p.cuts[1].input, PathBuf::from("/clips/b.OSV"));
        assert_eq!(p.duration(), 16.0);
        assert_eq!(p.cuts[1].keyframes.len(), 0);
        assert_eq!(p.cuts[0].keyframes[1].view.projection, Projection::LittlePlanet);
        std::fs::write(&path, r#"{"cuts": [{"input": "a.OSV", "in": 5, "out": 2}]}"#).unwrap();
        assert!(Project::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
