//! How open the view is around the camera, from a small level equirect
//! stitch: for `scan --image`, so an editor can find summits, clearings and
//! wide views without looking at every second of a clip.
//!
//! Sky is filled down from the zenith through pixels that look like it (blue,
//! or bright and grey like haze and cloud) and are smooth. Filling from the
//! top, rather than taking every sky-coloured pixel, keeps a blue tarp or a
//! lake from counting, and stops at the skyline even where snow below it is
//! as white as the clouds above, wherever the ridge between them has an edge.
//! Below the horizon it stops as well: the skyline from a summit dips only a
//! degree or two, and what lies lower is a lake's reflection, not sky.

/// Sky colour: blue clearly over red and about as strong as green, or
/// bright and nearly grey (haze, cloud).
fn looks_like_sky([r, g, b]: [f64; 3]) -> bool {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let blue = b > r + 0.05 && b > g - 0.03 && b > 0.35;
    let haze = max > 0.7 && max - min < 0.25 * max;
    blue || haze
}

/// Larger brightness steps to a neighbour (0..1, at about 1° a pixel) are
/// an edge: rock, a ridge, a tree, not sky.
const EDGE: f64 = 0.06;
/// A view out to the horizon in a direction: the skyline no higher than this.
const OPEN_DEG: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sky {
    /// Share of the upper hemisphere that is sky, by solid angle, in %.
    pub sky: f64,
    /// Median elevation of the skyline around the camera, degrees: near 0
    /// on a summit or open plateau, high in a valley or a forest.
    pub skyline: f64,
    /// Share of directions with a view out to the horizon (skyline within
    /// 5° of it), in %.
    pub open: f64,
}

/// Measures `rgb`, a level equirect `w` wide (`w`/2 high, yaw across, the
/// zenith on the top row).
pub fn measure(rgb: &[u8], w: usize) -> Sky {
    let h = w / 2;
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        [rgb[i] as f64 / 255.0, rgb[i + 1] as f64 / 255.0, rgb[i + 2] as f64 / 255.0]
    };
    let luma = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let rows = h / 2;
    // Sky-like and smooth: no edge to the next pixel across or down.
    let mut ok = vec![false; w * rows];
    for y in 0..rows {
        for x in 0..w {
            let c = px(x, y);
            let l = luma(c);
            let edge = (l - luma(px((x + 1) % w, y))).abs().max((l - luma(px(x, y + 1))).abs());
            ok[y * w + x] = looks_like_sky(c) && edge < EDGE;
        }
    }
    let mut sky = vec![false; w * rows];
    let mut todo: Vec<usize> = (0..w).filter(|&x| ok[x]).collect();
    for &i in &todo {
        sky[i] = true;
    }
    while let Some(i) = todo.pop() {
        let (x, y) = (i % w, i / w);
        let mut go = |j: usize| {
            if ok[j] && !sky[j] {
                sky[j] = true;
                todo.push(j);
            }
        };
        go(y * w + (x + 1) % w);
        go(y * w + (x + w - 1) % w);
        if y > 0 {
            go(i - w);
        }
        if y + 1 < rows {
            go(i + w);
        }
    }
    let elevation = |y: usize| 90.0 - (y as f64 + 0.5) * 180.0 / h as f64;
    let (mut area, mut whole) = (0.0, 0.0);
    for y in 0..rows {
        let weight = elevation(y).to_radians().cos();
        whole += weight * w as f64;
        area += weight * sky[y * w..(y + 1) * w].iter().filter(|&&s| s).count() as f64;
    }
    // The skyline in each direction: the lowest sky reached from above
    // without leaving it; straight up where the zenith itself is hidden.
    let mut line: Vec<f64> = (0..w)
        .map(|x| (0..rows).take_while(|&y| sky[y * w + x]).last().map_or(90.0, |y| elevation(y) - 90.0 / h as f64))
        .collect();
    let open = line.iter().filter(|&&e| e <= OPEN_DEG).count() as f64 / w as f64;
    line.sort_by(f64::total_cmp);
    Sky { sky: 100.0 * area / whole, skyline: line[w / 2], open: 100.0 * open }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blue sky over grey rock, with a ridge `ridge(yaw)` degrees high.
    fn scene(w: usize, ridge: impl Fn(f64) -> f64) -> Vec<u8> {
        let h = w / 2;
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let elevation = 90.0 - (y as f64 + 0.5) * 180.0 / h as f64;
                let yaw = x as f64 * 360.0 / w as f64;
                rgb.extend(if elevation > ridge(yaw) { [90, 150, 230] } else { [110, 100, 95] });
            }
        }
        rgb
    }

    #[test]
    fn a_summit_is_open_and_a_valley_is_not() {
        let summit = measure(&scene(360, |_| -1.0), 360);
        assert!(summit.sky > 99.0 && summit.skyline.abs() < 1.0 && summit.open == 100.0, "{summit:?}");
        // Valley walls 25° high either side, open along it (a quarter of
        // the way round each end).
        let valley = measure(&scene(360, |yaw| if (yaw % 180.0 - 90.0).abs() < 45.0 { 25.0 } else { 2.0 }), 360);
        assert!((valley.open - 50.0).abs() < 2.0, "{valley:?}");
        assert!(valley.sky < 85.0 && valley.skyline > 2.0, "{valley:?}");
    }

    #[test]
    fn a_blue_lake_below_is_not_sky() {
        let mut rgb = scene(360, |_| 10.0);
        // A blue lake from 30° below the horizon down, joined to nothing above.
        for y in 120..150 {
            for x in 0..360 {
                rgb[(y * 360 + x) * 3..][..3].copy_from_slice(&[60, 120, 200]);
            }
        }
        let s = measure(&rgb, 360);
        assert!(s.open == 0.0 && (s.skyline - 10.0).abs() < 1.5, "{s:?}");
    }
}
