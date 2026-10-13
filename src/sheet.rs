//! Contact sheets: many small stitched frames on one image, each labelled
//! with its clip time, so a whole clip can be skimmed at a glance (by a
//! person or by Claude, which reads images but not video).

use crate::stitch::Frame;

/// 5×7 glyphs, one byte per row, high bit on the left.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x70, 0x88, 0x98, 0xa8, 0xc8, 0x88, 0x70],
        '1' => [0x20, 0x60, 0x20, 0x20, 0x20, 0x20, 0x70],
        '2' => [0x70, 0x88, 0x08, 0x10, 0x20, 0x40, 0xf8],
        '3' => [0xf8, 0x10, 0x20, 0x10, 0x08, 0x88, 0x70],
        '4' => [0x10, 0x30, 0x50, 0x90, 0xf8, 0x10, 0x10],
        '5' => [0xf8, 0x80, 0xf0, 0x08, 0x08, 0x88, 0x70],
        '6' => [0x30, 0x40, 0x80, 0xf0, 0x88, 0x88, 0x70],
        '7' => [0xf8, 0x08, 0x10, 0x20, 0x40, 0x40, 0x40],
        '8' => [0x70, 0x88, 0x88, 0x70, 0x88, 0x88, 0x70],
        '9' => [0x70, 0x88, 0x88, 0x78, 0x08, 0x10, 0x60],
        '.' => [0, 0, 0, 0, 0, 0x60, 0x60],
        ':' => [0, 0x60, 0x60, 0, 0x60, 0x60, 0],
        '-' => [0, 0, 0, 0xf8, 0, 0, 0],
        's' => [0, 0, 0x70, 0x80, 0x70, 0x08, 0xf0],
        '#' => [0x50, 0xf8, 0x50, 0x50, 0xf8, 0x50, 0],
        _ => [0; 7],
    }
}

const SCALE: usize = 2;

/// Writes `text` with its top-left at (x, y), white on a dark box.
pub fn label(f: &mut Frame, x: usize, y: usize, text: &str) {
    let (cw, ch) = (6 * SCALE, 9 * SCALE);
    let w = text.chars().count() * cw + SCALE * 2;
    for py in y..(y + ch).min(f.height) {
        for px in x..(x + w).min(f.width) {
            let i = (py * f.width + px) * 3;
            f.rgb[i..i + 3].iter_mut().for_each(|v| *v /= 4);
        }
    }
    for (n, c) in text.chars().enumerate() {
        for (row, bits) in glyph(c).iter().enumerate() {
            for col in 0..5 {
                if bits & (0x80 >> col) == 0 {
                    continue;
                }
                for (dy, dx) in (0..SCALE).flat_map(|dy| (0..SCALE).map(move |dx| (dy, dx))) {
                    let (px, py) = (x + SCALE * 2 + n * cw + col * SCALE + dx, y + SCALE + row * SCALE + dy);
                    if px < f.width && py < f.height {
                        let i = (py * f.width + px) * 3;
                        f.rgb[i..i + 3].fill(255);
                    }
                }
            }
        }
    }
}

/// Marks an equirect tile with where yaw is −90, 0 and 90° and the horizon
/// (pitch 0), so a direction picked on the sheet becomes a view.
pub fn equirect_grid(f: &mut Frame) {
    let (w, h) = (f.width, f.height);
    let mut dot = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        let bright = f.rgb[i..i + 3].iter().map(|&v| v as u32).sum::<u32>() > 384;
        f.rgb[i..i + 3].fill(if bright { 0 } else { 255 });
    };
    for x in [w / 4, w / 2, 3 * w / 4] {
        for y in (0..h).filter(|y| y % 6 < 3) {
            dot(x, y);
        }
    }
    for x in (0..w).filter(|x| x % 6 < 3) {
        dot(x, h / 2);
    }
    for (x, text) in [(w / 4, "-90"), (w / 2, "0"), (3 * w / 4, "90")] {
        label(f, x + 2, h - 9 * SCALE - 2, text);
    }
}

const GAP: usize = 4;
/// A row's heading band and its waveform band, pixels.
const HEAD: usize = 9 * SCALE + 4;
const WAVE: usize = 16;

/// What goes with a row of a project's sheet: a heading over it, and
/// under it a waveform, one 0..1 peak per pixel across the sheet (`None`
/// where there is nothing), drawn from −40 to 0 dB.
pub struct Band {
    pub heading: String,
    pub wave: Option<Vec<Option<f32>>>,
}

/// How wide `compose` makes a sheet of `columns` tiles `tile` wide.
pub fn width(tile: usize, columns: usize) -> usize {
    columns * tile + (columns - 1) * GAP
}

/// Where tile column `i`'s middle is across the sheet.
pub fn tile_middle(tile: usize, i: usize) -> f64 {
    (i * (tile + GAP)) as f64 + tile as f64 / 2.0
}

/// `compose` with a heading over each row and the waveform under it.
pub fn compose_rows(tiles: &[Frame], columns: usize, bands: &[Band]) -> Frame {
    let sheet = compose(tiles, columns);
    let th = tiles[0].height;
    let wave = bands.iter().any(|b| b.wave.is_some());
    let row = HEAD + th + if wave { WAVE } else { 0 };
    let (width, height) = (sheet.width, bands.len() * row + bands.len().saturating_sub(1) * GAP);
    let mut out = Frame { width, height, rgb: vec![32; width * height * 3] };
    for (r, band) in bands.iter().enumerate() {
        let y0 = r * (row + GAP);
        // The heading on its own dark strip, the tiles under it.
        for y in y0..y0 + HEAD {
            out.rgb[y * width * 3..(y + 1) * width * 3].fill(16);
        }
        label(&mut out, 0, y0 + 2, &band.heading);
        let from = r * (th + GAP);
        for y in 0..th {
            let (src, dst) = ((from + y) * width * 3, (y0 + HEAD + y) * width * 3);
            out.rgb[dst..dst + width * 3].copy_from_slice(&sheet.rgb[src..src + width * 3]);
        }
        let Some(peaks) = &band.wave else { continue };
        let top = y0 + HEAD + th;
        for (x, p) in peaks.iter().enumerate().take(width) {
            let Some(p) = p else { continue };
            // In dB from −40, so a song mixed down still reads.
            let db = if *p > 0.0 { ((20.0 * p.log10() + 40.0) / 40.0).clamp(0.0, 1.0) } else { 0.0 };
            let half = ((db * (WAVE / 2) as f32).round() as usize).max(1);
            let mid = top + WAVE / 2;
            for y in mid - half.min(WAVE / 2)..(mid + half).min(top + WAVE) {
                let i = (y * width + x) * 3;
                out.rgb[i..i + 3].copy_from_slice(&[230, 150, 60]);
            }
        }
    }
    out
}

/// Lays `tiles` (all the same size) out in rows of `columns` with a gap.
pub fn compose(tiles: &[Frame], columns: usize) -> Frame {
    let (tw, th) = (tiles[0].width, tiles[0].height);
    let columns = columns.clamp(1, tiles.len());
    let rows = tiles.len().div_ceil(columns);
    let (width, height) = (columns * tw + (columns - 1) * GAP, rows * th + (rows - 1) * GAP);
    let mut rgb = vec![32; width * height * 3];
    for (n, tile) in tiles.iter().enumerate() {
        let (x0, y0) = ((n % columns) * (tw + GAP), (n / columns) * (th + GAP));
        for y in 0..th {
            let to = ((y0 + y) * width + x0) * 3;
            rgb[to..to + tw * 3].copy_from_slice(&tile.rgb[y * tw * 3..(y + 1) * tw * 3]);
        }
    }
    Frame { width, height, rgb }
}
