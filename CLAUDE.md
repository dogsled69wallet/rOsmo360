# rOsmo360

A Rust/Linux replacement for DJI Studio's playback of DJI Osmo 360 `.OSV` footage: correct dual-fisheye stitching, gyro stabilisation, rolling-shutter correction and free-look playback. The goal is to get as close to DJI Studio as possible without its proprietary pieces (AI tracking, its denoise, flow-based seam refinement).

The crate and binary are still called `osmo360`.

## Status (2026-10-09)

Working and verified on real footage:

- **Stitching** with DJI Studio's own default calibration, masks and blend ramps (via telemetry-parser). Seams are clean even with sub-metre parallax.
- **Stabilisation** from the camera's ~1 kHz fused attitude:
  - `horizon`: level horizon, heading follows smoothed camera heading.
  - `lock`: world-locked to the clip's starting orientation.
  - Per-sensor-row rolling-shutter correction (about 18 ms readout).
- **CLI**:
  - `rig`: dump calibration JSON.
  - `imu`: dump attitude CSV.
  - `stitch`: one frame to an equirect PNG.
  - `render`: span to an equirect MP4 with audio.
- **Real-time player** (`play`):
  - wgpu shader stitch.
  - Free look on yaw, pitch and roll.
  - Perspective, little-planet and flat equirect projections.
  - Plays the LRF proxy and swaps in a full-res frame on pause.

Not done yet, roughly in priority order:

1. **Zero-copy decode.** Import VA-API surfaces into Vulkan via dmabuf. This would allow full-res playback on the dev laptop.
2. **Audio in the player.**
3. **Selfie-stick / body masks.** telemetry-parser has `DjiMaskProfile` and stick masks; they are not wired in yet.
4. **Seam refinement for close subjects.** Expose the convergence variants (`--variant`) in the player; later, flow-based seams.
5. **Export from the player** with the current view or keyframed reframing to a flat MP4. Also write spherical metadata on equirect export.
6. **Colour.** D-Log M / LUT support, and 10-bit (P010) textures instead of NV12.
7. **Untested input.** Mouse-drag direction in the player has not been tried interactively; keys were tested with `wtype`.

## Commands

```sh
cargo build --release
B=target/release/osmo360
$B play  clip.OSV                                     # player (see `$B play --help` for keys)
$B stitch clip.OSV -t 10 -w 2048 --stabilize horizon -o f.png
$B render clip.OSV -s 48 -d 12 -w 1280 --stabilize horizon -o out.mp4
$B rig clip.OSV > rig.json;  $B imu clip.OSV > imu.csv
```

Debug flags on `stitch`/`render`:

- `--only 0|1`: render a single lens, to check overlap.
- `--variant N`: use an alternative calibration.
- `--no-rs`: disable rolling-shutter correction.
- `--imu-axes`, `--world-z-down`: override the IMU mapping (only for other bodies).

Player keys:

- Drag: look. Right-drag or Q/E: roll. ↑/↓: pitch. Shift+←/→: yaw.
- Scroll: zoom.
- Space: play/pause. ←/→: seek ±5 s. `,` `.`: frame step.
- S: stabilisation mode. R: rolling shutter. P: projection.
- 0: reset view. F: fullscreen.

Runtime dependencies: `ffmpeg` and `ffprobe` CLIs (VA-API for hardware decode).

## Layout

- `src/main.rs`: CLI. `Pipeline` holds the shared setup: rig, lenses, stabiliser, fps.
- `src/stitch.rs`: CPU reference stitcher. It is the ground truth the shader must match.
  - Lens projection: ThetaPoly with 5 k terms plus tangential.
  - Rasterised validity masks.
  - Blend weights.
  - Rolling-shutter re-aim.
- `src/stab.rs`: `Stabilizer::rig_from_view(t_ms, readout_ms, steps)`. Defines the `Mode` and `ImuAxes` enums.
- `src/imu.rs`: reads telemetry-parser's Quaternion/Gyro/Accel tags.
- `src/ffmpeg.rs`: RGB decode/encode through the ffmpeg CLI (used by `stitch`/`render`).
- `src/player/`:
  - `mod.rs`: winit app, playback clock, input.
  - `gpu.rs`: wgpu setup, uniforms, NV12 textures.
  - `shader.wgsl`: GPU port of `stitch.rs`.
  - `source.rs`: NV12 decode threads (LRF proxy / OSV via VA-API) and full-res stills.

## Hard-won facts (do not re-derive)

### File format

- `.OSV` is MP4 with:
  - two HEVC Main10 3840×3840 circular-fisheye tracks (one per lens),
  - AAC audio,
  - 2× `djmd` (protobuf, schema `dvtm_oq101.proto`),
  - 2× `dbgi`,
  - an MJPEG thumbnail.
- `.LRF` is a 2048×1024 H.264 proxy. It is not stitched: left half = OSV track 0 and right half = OSV track 1, each the full circle scaled to 1024² with no offset (checked by image difference).

### Calibration

Use telemetry-parser (git main, AdrianEddy) `util::camera_rig()`, not a hand decode. Its `src/dji/rig.rs` documents DJI Studio's behaviour:

- 24 calibration slots; the default is "rung 11".
- 5-coefficient ThetaPoly with DJI's tangential block.
- Blend of ±6.84° around 90°.
- 7 convergence variants.

A naive decode of slots 1 and 2 with 4 coefficients folds before 90° and is wrong.

### Geometry conventions

These come from crate `video-types`, `geometry.rs`:

- Rig frame: x right, y down, z forward.
- `Lens::rotation` is `q_rig_from_cam`.
- Equirect direction: `(cosφ sinλ, −sinφ, cosφ cosλ)`.

### IMU to rig mapping

telemetry-parser leaves `imu_from_rig` as identity for the Osmo 360, which is wrong. The verified mapping:

- The attitude is world_from_imu with world **+z up**.
- The IMU sits a **half turn about rig x** (`ImuAxes::X180`, the default).

How it was found:

- The car clip alone was misleading: identity also fit it, because the tilt was small and constant.
- On the handheld summit clip, world-lock across an 87° pan keeps the sun within about 1° only with X180. The same check also bounds attitude/video sync to under about 20 ms.
- Horizon mode is level at 30–50° of tilt.

### Decode performance on the dev laptop (Intel UHD 620)

- VA-API HEVC decode alone runs at about 3× real time per lens.
- The bottleneck is `scale_vaapi` + `hwdownload` + pipe of two 3840² tracks: 0.55× real time at full res, and only about 1.07× at 1920².
- So playback uses the proxy, and real full-res playback needs zero-copy.

### wgpu 30 surface format

Choose `Bgra8Unorm`. The first non-sRGB format offered is `Rgba16Float`, which the compositor treats as linear and washes out. The shader outputs display-encoded BT.709 and only linearises if forced onto an sRGB surface.

## How to verify changes

- **Stitch geometry:** run `stitch --only 0` and `--only 1`, then crop around longitude ±90°. Content must match across the overlap. Black areas are DJI's validity masks.
- **Stabilisation axes and sync:** run `--stabilize lock` across a fast pan and track a distant object or the sun; it must stay fixed. Horizon mode must keep the far horizon on latitude 0 at large tilts.
- **Player:** launch, then use `hyprctl clients -j` for the window title and geometry, `grim -g` to screenshot, and `wtype` to send keys. When sending multiple keys, don't separate them with spaces: space toggles play.

## Test footage (`~/Videos/osmo/`)

- `CAM_20260713102506_0002_D.{OSV,LRF}`: 70.7 s, driving. Close-range parallax inside the car; a 40° right turn at 51–57 s.
- `CAM_20260713153034_0037_D.{OSV,LRF}`: 55.5 s, handheld on a summit. Large tilts (at 5 s and 40 s), a fast 87° pan at 15–18 s, and the sun in frame.

## Conventions

- Match the existing style: `anyhow` errors, short doc comments that explain *why*, and no dead code.
- Commit messages explain the reasoning and evidence, not just the change.
- `sample-equirect.png` is deliberately untracked: it is personal footage.
