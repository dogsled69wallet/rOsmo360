//! Text overlays: typed text drawn into a transparent picture, which the
//! overlay track then moves, fades and grades like any other.
//!
//! The text is laid out and drawn by cosmic-text, iced's own text engine,
//! from the built-in fonts (in `fonts/`, each with its licence) or the
//! system's. A project keeps the font's name; where it's missing (another
//! machine, the Flatpak) the text falls back to Inter and says so. Sizes are
//! shares of the frame's height, so the text is drawn sharp at whatever size
//! the export or the view needs.

use crate::overlay::Picture;
use cosmic_text::{Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

/// The fonts that come with rOsmo360, by family; the first is the default.
pub const BUILT_IN: [&str; 6] = ["Inter", "Montserrat", "Bebas Neue", "Playfair Display", "Caveat", "Permanent Marker"];
const DEFAULT_FONT: &str = "Inter";

const FONT_FILES: [&[u8]; 9] = [
    include_bytes!("../fonts/Inter/Inter-Regular.ttf"),
    include_bytes!("../fonts/Inter/Inter-Bold.ttf"),
    include_bytes!("../fonts/Montserrat/Montserrat-Regular.ttf"),
    include_bytes!("../fonts/Montserrat/Montserrat-Bold.ttf"),
    include_bytes!("../fonts/BebasNeue/BebasNeue-Regular.ttf"),
    // The original variable font: Playfair Display's licence reserves its
    // name for unmodified copies, and cosmic-text sets its weight axis.
    include_bytes!("../fonts/PlayfairDisplay/PlayfairDisplay[wght].ttf"),
    include_bytes!("../fonts/Caveat/Caveat-Regular.ttf"),
    include_bytes!("../fonts/Caveat/Caveat-Bold.ttf"),
    include_bytes!("../fonts/PermanentMarker/PermanentMarker-Regular.ttf"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Text {
    pub content: String,
    #[serde(default = "default_font")]
    pub font: String,
    /// The letters' size (the font's em) as a share of the frame's height.
    #[serde(default = "default_size")]
    pub size: f64,
    #[serde(default = "white")]
    pub colour: Colour,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default)]
    pub align: TextAlign,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<Outline>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub shadow: bool,
}

impl Default for Text {
    /// What Add text starts with: bold white Inter with a soft shadow,
    /// readable over most footage.
    fn default() -> Self {
        Text { content: "Text".to_owned(), font: default_font(), size: default_size(), colour: white(), bold: true, align: TextAlign::Centre, outline: None, shadow: true }
    }
}

/// A line drawn around the letters, `width` as a share of the text's size.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outline {
    pub colour: Colour,
    pub width: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextAlign {
    Left,
    #[default]
    Centre,
    Right,
}

impl TextAlign {
    pub const ALL: [TextAlign; 3] = [TextAlign::Left, TextAlign::Centre, TextAlign::Right];
}

impl std::fmt::Display for TextAlign {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TextAlign::Left => "Left",
            TextAlign::Centre => "Centre",
            TextAlign::Right => "Right",
        })
    }
}

/// An sRGB colour, `"#rrggbb"` in JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour(pub [u8; 3]);

impl Colour {
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2])
    }

    pub fn parse(s: &str) -> Option<Colour> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        Some(Colour([(v >> 16) as u8, (v >> 8) as u8, v as u8]))
    }
}

impl Serialize for Colour {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Colour::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("{s:?} isn't a colour like \"#ffffff\"")))
    }
}

fn default_font() -> String {
    DEFAULT_FONT.to_owned()
}

fn default_size() -> f64 {
    0.08
}

fn white() -> Colour {
    Colour([255, 255, 255])
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The system's fonts and the built-in ones, loaded once: finding the
/// system's takes a moment.
fn fonts() -> &'static Mutex<(FontSystem, SwashCache)> {
    static FONTS: OnceLock<Mutex<(FontSystem, SwashCache)>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut system = FontSystem::new();
        for data in FONT_FILES {
            system.db_mut().load_font_data(data.to_vec());
        }
        // A variable font comes in at its default weight only, so bold
        // skipped it for another family: listed again as bold, it's chosen,
        // and drawn with its weight axis at 700.
        let db = system.db_mut();
        let variable: Vec<_> = db.faces().filter(|f| f.families.iter().any(|(n, _)| n == "Playfair Display") && f.weight == cosmic_text::fontdb::Weight::NORMAL).cloned().collect();
        for mut face in variable {
            face.weight = cosmic_text::fontdb::Weight::BOLD;
            db.push_face_info(face);
        }
        Mutex::new((system, SwashCache::new()))
    })
}

/// Every font family there is to choose: the built-in ones first, then the
/// system's, by name. Found once.
pub fn families() -> &'static [String] {
    static FAMILIES: OnceLock<Vec<String>> = OnceLock::new();
    FAMILIES.get_or_init(find_families)
}

fn find_families() -> Vec<String> {
    let fonts = lock();
    let mut system: Vec<String> = fonts.0.db().faces().filter_map(|f| f.families.first().map(|(name, _)| name.clone())).filter(|n| !BUILT_IN.contains(&n.as_str())).collect();
    system.sort_by_key(|n| n.to_lowercase());
    system.dedup();
    BUILT_IN.iter().map(|s| s.to_string()).chain(system).collect()
}

fn has_family(system: &FontSystem, name: &str) -> bool {
    system.db().faces().any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
}

/// The weight to ask `family` for: bold only where it has a bold face.
/// Asked for a weight it lacks, cosmic-text takes another family's (Bebas
/// Neue in bold came out as Noto Sans), so a one-weight font ignores Bold.
fn weight(system: &FontSystem, family: &str, bold: bool) -> Weight {
    let has_bold = system.db().faces().any(|f| f.weight.0 >= 600 && f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(family)));
    if bold && has_bold { Weight::BOLD } else { Weight::NORMAL }
}

/// The fonts, still usable if a thread drawing text panicked.
fn lock() -> std::sync::MutexGuard<'static, (FontSystem, SwashCache)> {
    fonts().lock().unwrap_or_else(|e| e.into_inner())
}

/// Draws `text` for a frame `frame_height` pixels high: a premultiplied
/// picture just big enough for it, its outline and shadow, with its size
/// on the frame (`Picture::natural`). Also says which font was missing, if
/// it fell back to Inter.
pub fn render(text: &Text, frame_height: usize) -> (Picture, Option<String>) {
    let mut guard = lock();
    let (system, cache) = &mut *guard;
    let missing = (!has_family(system, &text.font)).then(|| text.font.clone());
    let family = if missing.is_some() { DEFAULT_FONT } else { &text.font };
    let px = (text.size * frame_height as f64).max(2.0) as f32;
    let attrs = Attrs::new().family(Family::Name(family)).weight(weight(system, family, text.bold));
    let mut buffer = Buffer::new(system, Metrics::new(px, px * 1.2));
    let content = if text.content.is_empty() { " " } else { &text.content };
    let align = match text.align {
        TextAlign::Left => Align::Left,
        TextAlign::Centre => Align::Center,
        TextAlign::Right => Align::Right,
    };
    buffer.set_size(system, None, None);
    buffer.set_text(system, content, &attrs, Shaping::Advanced, Some(align));
    buffer.shape_until_scroll(system, false);
    // Laid out unbounded, then again as wide as the widest line, so the
    // lines align within it.
    let width = buffer.layout_runs().map(|r| r.line_w).fold(1.0f32, f32::max).ceil();
    buffer.set_size(system, Some(width), None);
    buffer.shape_until_scroll(system, false);
    let height = buffer.layout_runs().map(|r| r.line_top + r.line_height).fold(px, f32::max).ceil();
    // Room around the letters for the outline and the shadow.
    let outline = text.outline.map(|o| (o.width * px as f64).max(0.0));
    let ring = outline.unwrap_or(0.0);
    let (shadow_off, shadow_blur) = if text.shadow { (0.05 * px as f64, 0.06 * px as f64) } else { (0.0, 0.0) };
    let pad = (ring + shadow_off + 2.0 * shadow_blur).ceil() as usize + 2;
    let (w, h) = (width as usize + 2 * pad, height as usize + 2 * pad);
    let mut mask = vec![0f32; w * h];
    buffer.draw(system, cache, Color::rgb(255, 255, 255), |x, y, gw, gh, c| {
        let a = c.a() as f32 / 255.0;
        for yy in y..y + gh as i32 {
            for xx in x..x + gw as i32 {
                let (px, py) = (xx + pad as i32, yy + pad as i32);
                if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                    let m = &mut mask[py as usize * w + px as usize];
                    *m = (*m + a).min(1.0);
                }
            }
        }
    });
    drop(guard);
    let around = outline.map(|r| dilate(&mask, (w, h), r)).unwrap_or_else(|| mask.clone());
    let shade = text.shadow.then(|| blur(&shift(&around, (w, h), shadow_off), (w, h), shadow_blur));
    let fill = text.colour.0.map(|c| c as f32 / 255.0);
    let ring_colour = text.outline.map_or([0.0; 3], |o| o.colour.0.map(|c| c as f32 / 255.0));
    let mut rgba = vec![0u8; w * h * 4];
    rgba.par_chunks_mut(4).enumerate().for_each(|(i, px)| {
        // Shadow, then outline, then the letters, each over what's below.
        let mut c = [0f32; 3];
        let mut a = 0f32;
        let mut over = |colour: [f32; 3], alpha: f32| {
            for k in 0..3 {
                c[k] = colour[k] * alpha + c[k] * (1.0 - alpha);
            }
            a = alpha + a * (1.0 - alpha);
        };
        if let Some(s) = &shade {
            over([0.0; 3], 0.6 * s[i]);
        }
        if outline.is_some() {
            over(ring_colour, around[i]);
        }
        over(fill, mask[i]);
        for k in 0..3 {
            px[k] = (c[k] * 255.0).round() as u8;
        }
        px[3] = (a * 255.0).round() as u8;
    });
    let picture = Picture { width: w, height: h, rgba, natural: Some(h as f64 / frame_height as f64) };
    (picture, missing)
}

/// `mask` grown by `r` pixels all round (a disc), soft at its edge.
fn dilate(mask: &[f32], (w, h): (usize, usize), r: f64) -> Vec<f32> {
    let reach = r.ceil() as i32;
    let offsets: Vec<(i32, i32, f32)> = (-reach..=reach)
        .flat_map(|dy| (-reach..=reach).map(move |dx| (dx, dy)))
        .filter_map(|(dx, dy)| {
            let d = ((dx * dx + dy * dy) as f64).sqrt();
            // Full inside the radius, fading over its last pixel.
            let k = (r + 0.5 - d).clamp(0.0, 1.0) as f32;
            (k > 0.0).then_some((dx, dy, k))
        })
        .collect();
    (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = ((i % w) as i32, (i / w) as i32);
            let mut best = 0f32;
            for &(dx, dy, k) in &offsets {
                let (sx, sy) = (x + dx, y + dy);
                if sx >= 0 && sy >= 0 && (sx as usize) < w && (sy as usize) < h {
                    best = best.max(mask[sy as usize * w + sx as usize] * k);
                }
            }
            best
        })
        .collect()
}

/// `mask` moved `by` pixels right and down.
fn shift(mask: &[f32], (w, h): (usize, usize), by: f64) -> Vec<f32> {
    let d = by.round() as usize;
    let mut out = vec![0f32; w * h];
    for y in d..h {
        out[y * w + d..(y + 1) * w].copy_from_slice(&mask[(y - d) * w..(y - d + 1) * w - d]);
    }
    out
}

/// A box blur of radius `r` across and then down, twice: near enough a
/// Gaussian for a shadow.
fn blur(mask: &[f32], (w, h): (usize, usize), r: f64) -> Vec<f32> {
    let r = r.round() as usize;
    if r == 0 {
        return mask.to_vec();
    }
    let mut a = mask.to_vec();
    for _ in 0..2 {
        a = box_rows(&a, w, h, r);
        a = transpose(&box_rows(&transpose(&a, w, h), h, w, r), h, w);
    }
    a
}

fn box_rows(m: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let src = &m[y * w..(y + 1) * w];
        for (x, o) in row.iter_mut().enumerate() {
            let (a, b) = (x.saturating_sub(r), (x + r).min(w - 1));
            *o = src[a..=b].iter().sum::<f32>() / (2 * r + 1) as f32;
        }
    });
    out
}

fn transpose(m: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            out[x * h + y] = m[y * w + x];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_draws_with_its_font_and_falls_back() {
        let text = Text { content: "Kyrkja".to_owned(), ..Default::default() };
        let (pic, missing) = render(&text, 1080);
        assert!(missing.is_none());
        // About the size asked for: 8% of 1080 is 86 px, a line 1.2 that.
        assert!(pic.height > 90 && pic.height < 140, "{}×{}", pic.width, pic.height);
        assert!(pic.width > 3 * pic.height / 2);
        assert!(pic.rgba.chunks(4).any(|p| p[3] == 255) && pic.rgba.chunks(4).any(|p| p[3] == 0));
        let (_, missing) = render(&Text { font: "No Such Font".to_owned(), ..text.clone() }, 1080);
        assert_eq!(missing.as_deref(), Some("No Such Font"));
        // Bold is heavier, even from the variable Playfair Display.
        let ink = |t: &Text| render(t, 540).0.rgba.chunks(4).map(|p| p[3] as u64).sum::<u64>();
        let playfair = Text { font: "Playfair Display".to_owned(), shadow: false, bold: false, ..text.clone() };
        assert!(ink(&Text { bold: true, ..playfair.clone() }) > ink(&playfair) * 11 / 10);
        // JSON: colours as hex, defaults left out.
        let json = serde_json::to_string(&Text { outline: Some(Outline { colour: Colour([0, 0, 0]), width: 0.1 }), ..text }).unwrap();
        assert!(json.contains(r##""colour":"#ffffff""##) && json.contains(r##""outline":{"colour":"#000000","width":0.1}"##), "{json}");
        assert_eq!(Colour::parse("#ff8000"), Some(Colour([255, 128, 0])));
    }
}

#[cfg(test)]
mod faces {
    use super::*;

    /// Bold Playfair Display is drawn from its own variable font: once it
    /// was quietly Noto Sans, the ink test passing on two fallbacks.
    #[test]
    fn bold_comes_from_the_family_asked_for() {
        let mut guard = lock();
        let (system, _) = &mut *guard;
        for family in BUILT_IN {
            for bold in [false, true] {
                let attrs = Attrs::new().family(Family::Name(family)).weight(weight(system, family, bold));
                let mut buffer = Buffer::new(system, Metrics::new(40.0, 48.0));
                buffer.set_text(system, "Pa", &attrs, Shaping::Advanced, None);
                buffer.shape_until_scroll(system, false);
                let used: Vec<String> = buffer.layout_runs().flat_map(|r| r.glyphs.iter().map(|g| g.font_id)).filter_map(|id| system.db().face(id).map(|f| f.families[0].0.clone())).collect();
                assert!(!used.is_empty() && used.iter().all(|f| f == family), "{family} bold {bold}: {used:?}");
            }
        }
    }
}
