# rOsmo360

A Rust/Linux replacement for DJI Studio's playback of DJI Osmo 360 `.OSV` footage: correct dual-fisheye stitching, gyro stabilisation, rolling-shutter correction and free-look playback. The goal is to get as close to DJI Studio as possible without its proprietary pieces (AI tracking, its denoise, flow-based seam refinement).

The crate and binary are still called `osmo360`.

## Status (2026-10-10)

Working and verified on real footage:

- **Stitching** with DJI Studio's own default calibration, masks and blend ramps (via telemetry-parser), plus two per-frame seam corrections:
  - Exposure matching: evens out the lenses' brightness and colour along the seam.
  - Seam alignment: shifts each lens half-way across the seam where close subjects (legs, the hand on the selfie stick) would otherwise ghost.
- **Stabilisation** from the camera's ~1 kHz fused attitude:
  - `horizon`: level horizon, heading follows smoothed camera heading.
  - `lock`: world-locked to the clip's starting orientation.
  - Per-sensor-row rolling-shutter correction (about 18 ms readout).
- **CLI**:
  - `rig`: dump calibration JSON.
  - `imu`: dump attitude CSV.
  - `stitch`: one frame to an equirect PNG.
  - `render`: span to an equirect MP4 with audio.
- **Real-time player** (`play`), an iced GUI:
  - Open dialog (Ctrl+O), seek bar, and pickers for stabilisation, projection, playback source, FOV and rolling shutter.
  - wgpu shader stitch, drawn as an iced shader widget.
  - Free look on yaw, pitch and roll.
  - Perspective, little-planet and flat equirect projections.
  - Plays the LRF proxy (or the OSV at 1920 px or full size) and swaps in a full-res frame on pause.
- **Hardware decode on any GPU vendor:** the first of Vulkan Video, VA-API and CUDA that decodes a test frame, else software. `--hwaccel` forces one.

Not done yet, roughly in priority order:

1. **Zero-copy decode.** Hand Vulkan Video frames to wgpu without the CPU round trip. This would allow full-res playback on the dev laptop.
2. **Audio in the player.**
3. **Selfie-stick / body masks.** telemetry-parser has `DjiMaskProfile` and stick masks; they are not wired in yet.
4. **Seam alignment, next steps.** Alignment searches only across the seam (α); add a small φ search if lateral misalignment shows up. Seam analysis runs on the UI thread per frame; move it to the decode threads if it costs frames on slower machines.
5. **Export from the player** with the current view or keyframed reframing to a flat MP4. Also write spherical metadata on equirect export.
6. **Colour.** D-Log M / LUT support, and 10-bit (P010) textures instead of NV12.
7. **Untested input.** The iced GUI's mouse drag, keys, pause/full-res still and Open dialog have not been driven interactively (no key-injection tool on the KDE machine).

## Commands

```sh
cargo build --release
B=target/release/osmo360
$B play [clip.OSV]                                    # player (see `$B play --help` for keys)
$B stitch clip.OSV -t 10 -w 2048 --stabilize horizon -o f.png
$B render clip.OSV -s 48 -d 12 -w 1280 --stabilize horizon -o out.mp4
$B rig clip.OSV > rig.json;  $B imu clip.OSV > imu.csv
```

Debug flags on `stitch`/`render`:

- `--only 0|1`: render a single lens, to check overlap.
- `--variant N`: use an alternative calibration.
- `--no-rs`: disable rolling-shutter correction.
- `--no-exposure`, `--no-align`: disable the seam corrections, to compare.
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
- `src/ring.rs`: the seam ring: azimuth φ around lens 0's axis and angle α from it.
- `src/exposure.rs`: per-frame exposure matching (`Seam::measure`, `Seam::gain`).
- `src/align.rs`: per-frame seam alignment (`Aligner::measure`, `Aligner::warp`).
- The shader's `exposure_gain` and `align_warp` mirror `Seam::gain` and `Aligner::warp`; change them together.
- `src/imu.rs`: reads telemetry-parser's Quaternion/Gyro/Accel tags.
- `src/ffmpeg.rs`: RGB decode/encode through the ffmpeg CLI (used by `stitch`/`render`), plus `Hwaccel` probing and the player's NV12 decode command.
- `src/player/`:
  - `mod.rs`: iced app: clip loading, widgets, keys, and the `Sphere` shader widget (drag/scroll to look).
  - `playback.rs`: playback clock, streams and full-res stills.
  - `gpu.rs`: iced shader `Primitive`/`Pipeline`: uniforms and NV12 textures, uploaded once per frame id.
  - `shader.wgsl`: GPU port of `stitch.rs`.
  - `source.rs`: NV12 decode threads (LRF proxy / OSV) and full-res stills.

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

### Attitude/frame timing

- A frame's timestamp is the **start** of its readout (18.3 ms, top-down), so the frame's middle row is at t + 9.15 ms. `rig_from_view` reads the attitude from t to t + readout.
- Centring the readout on t instead made the picture swim: world-locked distant terrain moved 0.13–0.48° per frame. Sweeping a time offset gave a sharp minimum at 9 ms on the summit clip (0.134° → 0.018° yaw step RMS); clips 0026 and 0019 agreed.
- The ~1 kHz attitude often repeats the previous value (to float noise) for several ms and then jumps, even at 20–60°/s. Dropping the repeats and interpolating did **not** reduce jitter, so this is not the cause of wobble.
- Measure wobble by rendering `--stabilize lock` at `-w 1024` and phase-correlating the band from the horizon to 25° below between consecutive frames. The RMS step in degrees is the number to compare.

### Seam

- The lenses expose differently: 5% in clear sky, 15–20% in sun haze (clip 0043). The fixed blend turns that into a light or dark band; it is not vignetting (single-lens profiles show no edge falloff). DJI's metadata has no vignetting or shading coefficients, only a mode number.
- Exposure gains: the median log ratio per azimuth bin, half applied to each lens, fading in from 60° off-axis. They are display-space RGB gains.
- Ghosting from close subjects is parallax across the seam (along α). The convergence variants don't fix it: no single distance suits both a hand at 0.5 m and mountains. A hard cut where the lenses disagree drew lines in the sky (before exposure matching) and was dropped.
- Alignment matches zero-mean NCC patches symmetrically about the output angle (lens 0 at α − δ/2, lens 1 at α + δ/2), so the field needs no inversion. Featureless patches fall back to δ = 0, the calibration's own alignment.
- telemetry-parser's `DjiMaskProfile` covers the housing and lens guards, not the stick or hands.

### Hardware decode through the ffmpeg CLI

- Use `-hwaccel X -hwaccel_output_format X` then `hwdownload,format=F` with exactly one format: `p010le` for the 10-bit OSV, `nv12` for the 8-bit LRF. Listing both fails.
- That chain fails loudly when the GPU can't decode, because `hwdownload` rejects software frames; plain `-hwaccel` silently falls back. `Hwaccel::pick` relies on this.
- Convert to NV12 and scale on the CPU. GPU filters are not portable: on NVIDIA, `scale_vaapi` (nvidia-vaapi-driver) and `scale_vulkan` to NV12 fail, and `scale_cuda` was slower. The cost is decode plus download, not conversion.
- RTX 3090: Vulkan decodes one lens at 3.2× real time (CUDA 3.7×). Both lenses at full 3840² run at about 1.1× (Vulkan) to 1.3× (CUDA) real time, which is enough for full-res playback.

### Decode performance on the dev laptop (Intel UHD 620)

- VA-API HEVC decode alone runs at about 3× real time per lens.
- The bottleneck is `scale_vaapi` + `hwdownload` + pipe of two 3840² tracks: 0.55× real time at full res, and only about 1.07× at 1920².
- So playback uses the proxy, and real full-res playback needs zero-copy.

### iced and wgpu

- iced 0.14 is on wgpu 27. Use its re-export `iced::wgpu`; never add a direct `wgpu` dependency, or the versions split.
- iced picks the surface format. The shader outputs display-encoded BT.709 and linearises only when the target is sRGB (`misc.y`).
- The widget draws into a sub-rectangle, so the shader subtracts its framebuffer origin (`misc.zw`) from `@builtin(position)`.

## How to verify changes

- **Stitch geometry:** run `stitch --only 0` and `--only 1`, then crop around longitude ±90°. Content must match across the overlap. Black areas are DJI's validity masks.
- **Stabilisation axes and sync:** run `--stabilize lock` across a fast pan and track a distant object or the sun; it must stay fixed. Horizon mode must keep the far horizon on latitude 0 at large tilts.
- **Player:** launch, then screenshot the active window. On KDE use `spectacle -b -n -a -o out.png`. On Hyprland use `hyprctl clients -j` for geometry, `grim -g` to screenshot and `wtype` to send keys; don't separate multiple keys with spaces, because space toggles play. Compare the seek-bar time across two screenshots to check real-time playback.

## Test footage (`/mnt/media/VirtualShare/FjellturJuli2026/Kyrkja/`)

35 clips from one hike. 0036 (13.5 min) and 0047 (7.4 min) are long, which is useful for seek and long playback.

- `CAM_20260713102506_0002_D.{OSV,LRF}`: 70.7 s, driving. Close-range parallax inside the car; a 40° right turn at 51–57 s.
- `CAM_20260713153034_0037_D.{OSV,LRF}`: 55.5 s, handheld on a summit. Large tilts (at 5 s and 40 s), a fast 87° pan at 15–18 s, and the sun in frame.

## Conventions

- Match the existing style: `anyhow` errors, short doc comments that explain *why*, and no dead code.
- Commit messages explain the reasoning and evidence, not just the change.
- `sample-equirect.png` is deliberately untracked: it is personal footage.
