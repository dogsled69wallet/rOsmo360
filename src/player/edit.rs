//! The timeline's edit: cuts played one after another, the music under them
//! and the mix, with undo. Plain data, so the edits can be checked without a
//! window.
//!
//! Edit time runs along the timeline from 0 to the edit's length; each cut
//! covers its stretch of it, and clip time is where in its own clip that is.

use crate::project::{Cut, Music, Sound};

/// The shortest a split or trim may leave a cut, in seconds.
pub const MIN_CUT: f64 = 0.2;

/// What undo brings back: everything an edit changes.
type Snapshot = (Vec<Cut>, Music, Sound);

#[derive(Default)]
pub struct Edit {
    pub cuts: Vec<Cut>,
    pub music: Music,
    pub sound: Sound,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Counts changes, so the app can tell whether the edit was saved.
    pub version: u64,
}

impl Edit {
    pub fn duration(&self) -> f64 {
        self.cuts.iter().map(|c| c.end - c.start).sum()
    }

    /// The timeline's length: the video's, or the music's while it runs on
    /// beyond it (the frame still to fill). The export ends with the video.
    pub fn frame(&self) -> f64 {
        self.duration().max(self.music.end())
    }

    /// Where cut `i` starts on the timeline.
    pub fn offset(&self, i: usize) -> f64 {
        self.cuts[..i].iter().map(|c| c.end - c.start).sum()
    }

    /// The cut at edit time `t` and the clip time there. The end of the edit
    /// is the last cut's out point.
    pub fn locate(&self, t: f64) -> Option<(usize, f64)> {
        let mut from = 0.0;
        for (i, c) in self.cuts.iter().enumerate() {
            let len = c.end - c.start;
            if t < from + len || i == self.cuts.len() - 1 {
                return Some((i, (c.start + (t - from).max(0.0)).min(c.end)));
            }
            from += len;
        }
        None
    }

    pub fn edit_time(&self, i: usize, clip_t: f64) -> f64 {
        self.offset(i) + (clip_t - self.cuts[i].start)
    }

    fn snapshot(&self) -> Snapshot {
        (self.cuts.clone(), self.music.clone(), self.sound)
    }

    fn restore(&mut self, (cuts, music, sound): Snapshot) -> Snapshot {
        let now = self.snapshot();
        (self.cuts, self.music, self.sound) = (cuts, music, sound);
        now
    }

    /// Remembers the edit as it is, for undo; call before each change.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        self.redo.clear();
        self.version += 1;
    }

    pub fn undo(&mut self) -> bool {
        let Some(then) = self.undo.pop() else { return false };
        let now = self.restore(then);
        self.redo.push(now);
        self.version += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(then) = self.redo.pop() else { return false };
        let now = self.restore(then);
        self.undo.push(now);
        self.version += 1;
        true
    }

    /// Starts over with `cuts` and their music and mix (an opened or new
    /// project), forgetting undo.
    pub fn replace(&mut self, cuts: Vec<Cut>, music: Music, sound: Sound) {
        self.cuts = cuts;
        self.music = music;
        self.sound = sound;
        self.undo.clear();
        self.redo.clear();
        self.version += 1;
    }

    /// Splits cut `i` at clip time `t`. Both halves keep all its keyframes,
    /// so the view runs on across the split unchanged, and each half can be
    /// reframed on its own from then on.
    pub fn split(&mut self, i: usize, t: f64) -> bool {
        let c = &self.cuts[i];
        if t < c.start + MIN_CUT || t > c.end - MIN_CUT {
            return false;
        }
        let mut second = c.clone();
        second.start = t;
        self.cuts[i].end = t;
        self.cuts.insert(i + 1, second);
        true
    }

    /// Moves cut `i`'s in point (`start`) or out point to clip time `t`,
    /// kept within the clip (`0..clip_len`) and at least `MIN_CUT` long.
    pub fn trim(&mut self, i: usize, start: bool, t: f64, clip_len: f64) {
        let c = &mut self.cuts[i];
        if start {
            c.start = t.clamp(0.0, c.end - MIN_CUT);
        } else {
            c.end = t.clamp(c.start + MIN_CUT, clip_len);
        }
    }

    /// Moves cut `from` so it plays at position `to` (counted before the move).
    pub fn move_cut(&mut self, from: usize, to: usize) {
        let cut = self.cuts.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.cuts.insert(to.min(self.cuts.len()), cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cut(name: &str, start: f64, end: f64) -> Cut {
        Cut { input: PathBuf::from(name), start, end, keyframes: Default::default(), stabilize: None, note: None }
    }

    fn edit() -> Edit {
        Edit { cuts: vec![cut("a", 10.0, 14.0), cut("b", 2.0, 5.0)], ..Default::default() }
    }

    #[test]
    fn maps_edit_time_to_cuts() {
        let e = edit();
        assert_eq!(e.duration(), 7.0);
        assert_eq!(e.locate(1.0), Some((0, 11.0)));
        assert_eq!(e.locate(4.5), Some((1, 2.5)));
        assert_eq!(e.locate(9.0), Some((1, 5.0)));
        assert_eq!(e.edit_time(1, 3.0), 5.0);
    }

    #[test]
    fn splits_trims_moves_and_undoes() {
        let mut e = edit();
        e.checkpoint();
        assert!(e.split(0, 12.0));
        assert!(!e.split(0, 11.9));
        assert_eq!((e.cuts[0].end, e.cuts[1].start, e.cuts.len()), (12.0, 12.0, 3));
        e.trim(2, false, 99.0, 6.0);
        e.trim(1, true, 13.95, 20.0);
        assert_eq!((e.cuts[2].end, e.cuts[1].start), (6.0, 13.8));
        e.move_cut(2, 0);
        assert_eq!(e.cuts[0].input, PathBuf::from("b"));
        e.move_cut(0, 3);
        assert_eq!(e.cuts[2].input, PathBuf::from("b"));
        assert!(e.undo());
        assert_eq!(e.cuts.len(), 2);
        assert!(e.redo());
        assert_eq!(e.cuts.len(), 3);
    }
}
