//! What the Osmo 360 records about a clip beyond what telemetry-parser hands
//! us: the colour profile. DJI tags D-Log M video BT.709 like any other, so
//! only its own metadata (`djmd`, protobuf `dvtm_oq101.proto`) tells.

use crate::ffmpeg;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;

/// The colour profile a clip was filmed in (`dvtm_library.proto`'s
/// `ColorModeType`): what decides whether it needs a LUT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    #[default]
    Normal,
    DLogM,
    /// Another of DJI's profiles, by its number (D-Log 2, HLG 9, D-Log 2 22).
    Other(u32),
}

impl ColorMode {
    pub fn from_number(n: u32) -> Self {
        match n {
            0 => ColorMode::Normal,
            19 => ColorMode::DLogM,
            n => ColorMode::Other(n),
        }
    }

    pub fn number(self) -> u32 {
        match self {
            ColorMode::Normal => 0,
            ColorMode::DLogM => 19,
            ColorMode::Other(n) => n,
        }
    }

    pub fn name(self) -> String {
        match self {
            ColorMode::Normal => "normal".to_owned(),
            ColorMode::DLogM => "D-Log M".to_owned(),
            ColorMode::Other(2) => "D-Log".to_owned(),
            ColorMode::Other(9) => "HLG".to_owned(),
            ColorMode::Other(22) => "D-Log 2".to_owned(),
            ColorMode::Other(n) => format!("colour mode {n}"),
        }
    }
}

/// `input`'s colour profile, from `StreamMeta.color_mode` (field 4, its
/// field 1) in the first `djmd` sample: ProductMeta's field 2. A clip
/// without one (left out, as proto3 leaves out zero) is normal colour.
pub fn color_mode(input: &Path) -> Result<ColorMode> {
    // The first djmd stream among the data streams (dbgi ones follow).
    let tags = ffmpeg::probe(input, "d", "stream=codec_tag_string")?;
    let Some(n) = tags.lines().position(|t| t.trim() == "djmd") else {
        return Ok(ColorMode::Normal);
    };
    let out = ffmpeg::tool("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(input)
        .args(["-map", &format!("0:d:{n}"), "-frames:d", "1", "-c", "copy", "-f", "data", "-"])
        .stderr(Stdio::null())
        .output()
        .context("running ffmpeg")?;
    Ok(mode_in(&out.stdout))
}

/// The colour mode in one `djmd` sample (a ProductMeta).
fn mode_in(sample: &[u8]) -> ColorMode {
    let mut mode = 0;
    for (f, v) in fields(sample) {
        if let (2, Value::Bytes(stream)) = (f, v) {
            for (f, v) in fields(stream) {
                if let (4, Value::Bytes(colour)) = (f, v) {
                    for (f, v) in fields(colour) {
                        if let (1, Value::Int(m)) = (f, v) {
                            mode = m as u32;
                        }
                    }
                }
            }
        }
    }
    ColorMode::from_number(mode)
}

enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// A protobuf message's fields, as far as they parse.
fn fields(mut b: &[u8]) -> impl Iterator<Item = (u64, Value<'_>)> {
    std::iter::from_fn(move || {
        let key = varint(&mut b)?;
        let value = match key & 7 {
            0 => Value::Int(varint(&mut b)?),
            2 => {
                let n = varint(&mut b)? as usize;
                let (v, rest) = (b.get(..n)?, b.get(n..)?);
                b = rest;
                Value::Bytes(v)
            }
            1 => {
                b = b.get(8..)?;
                Value::Fixed
            }
            5 => {
                b = b.get(4..)?;
                Value::Fixed
            }
            _ => return None,
        };
        Some((key >> 3, value))
    })
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&x, rest) = b.split_first()?;
        *b = rest;
        v |= ((x & 0x7f) as u64) << shift;
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ProductMeta { stream_meta (2) { color_mode (4) { color_mode (1): 19 } } },
    /// among other fields of each kind.
    #[test]
    fn reads_the_colour_mode() {
        let colour = [0x08, 19];
        let mut stream = vec![0x0d, 1, 2, 3, 4, 0x22, colour.len() as u8];
        stream.extend(colour);
        let mut product = vec![0x08, 0x96, 0x01, 0x12, stream.len() as u8];
        product.extend(&stream);
        assert_eq!(mode_in(&product), ColorMode::DLogM);
        assert_eq!(mode_in(&[]), ColorMode::Normal);
        // Cut short, it stops instead of reading past the end.
        assert!(fields(&product[..product.len() - 1]).count() <= 2);
    }
}
