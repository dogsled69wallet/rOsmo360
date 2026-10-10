# rOsmo360

A Rust/Linux replacement for DJI Studio's playback of DJI Osmo 360 `.OSV` footage: correct dual-fisheye stitching, gyro stabilisation, rolling-shutter correction and free-look playback. The goal is to get as close to DJI Studio as possible without its proprietary pieces (AI tracking, its denoise, flow-based seam refinement).

The tool is called **rOsmo360** (the r is for its Rust core); the window title, app id and `--help` use that name. The crate and binary are `rosmo360` (renamed from `osmo360` on 2026-10-10).

## Status (2026-10-10, version 0.2.6)

Working and verified on real footage:

- **Stitching** with DJI Studio's own default calibration, masks and blend ramps (via telemetry-parser), plus two per-frame seam corrections:
  - Exposure matching: evens out the lenses' brightness and colour along the seam.
  - Seam alignment: shifts each lens half-way across the seam where close subjects (legs, the hand on the selfie stick) would otherwise ghost.
  - Disagreement cut: where the lenses still disagree after alignment, cut to the lens owning that side of the seam instead of blending, so what can't be aligned (the hand on the stick) is solid rather than see-through.
- **Stabilisation** from the camera's ~1 kHz fused attitude:
  - `horizon`: level horizon, heading follows smoothed camera heading.
  - `lock`: world-locked to the clip's starting orientation.
  - Per-sensor-row rolling-shutter correction (about 18 ms readout).
- **CLI**:
  - `rig`: dump calibration JSON.
  - `imu`: dump attitude CSV.
  - `stitch`: one frame to an equirect PNG.
  - `render`: span to an equirect MP4 with audio.
  - `seam`: seam alignment quality over a span (ghosting left, frame-to-frame wobble).
  - `export`: a framed video with sound (MP4, or `--format resolve`: DNxHR HQ + PCM in MOV, since Resolve free on Linux reads no H.264/H.265/AAC; DNxHR encodes any size, ~27 MB/s at 1080p), from a fixed `--view` or a keyframed `--reframe` JSON (`view::Reframe`), or a project JSON of cuts from several clips (`project::Project`), joined with their sound. A single clip is exported as a one-cut project. `--clips` writes a file per cut into the folder `-o`, named `<project or clip> - 001.<ext>`, with `--handles` seconds of extra footage either side where the clip has it (`main::Target::Clips`). 1080p by default (`-r` 720p–2160p is the short side; `-a` aspect, or the reframe's `"aspect"`). `--draft` stitches from the LRF proxy.
  - `preview`: a contact sheet of stitched moments labelled with time; equirect tiles marked at yaw −90/0/90° and the horizon, or the reframe's framing. Given a project, a row per cut (`--columns` tiles spread across it, or `--every N` wrapping rows) framed as the export frames it, labelled `#cut time`; `cut_pipelines` is shared with `export`.
  - `scan`: CSV per second of heading, tilt, turning, shake, loudness and peak.
  - `info`: duration, fps, recording time and contents as JSON.
- **Real-time player** (`play`), an iced GUI:
  - Open dialog (Ctrl+O), seek bar, and pickers for stabilisation, projection, playback source, FOV and rolling shutter.
  - wgpu shader stitch, drawn as an iced shader widget.
  - Free look on yaw, pitch and roll.
  - Perspective, little-planet and flat equirect projections.
  - Plays the LRF proxy (or the OSV at 1920 px or full size) and swaps in a full-res frame on pause, unless "Full res when paused" is off (then paused frames stay at the source's quality, which keeps keyframing steady).
  - Sound through cpal, with a mute icon and volume slider. While sound plays it is the playback clock.
  - Aspect lock (Free, 16:9, 9:16, 4:3, 1:1, 2:1) letterboxes the view; a saved view and the export take the same shape. It sits in the top bar with Export… and the export resolution.
  - Settings are remembered between runs (`settings.rs`).
  - Shows the Vibing with Ravelund logo while a clip loads, and a "Made by" credit line at the bottom. The logo is `vibewithravelund-lowres-corrected.jpg`, embedded with `include_bytes!`, so it must stay in the repo root.
  - Follows the OS theme and accent through native-theme (KDE colour scheme, GTK/portal, Windows, macOS), live when it changes. iced's dark theme is the fallback.
  - Save the paused frame as PNG: the view as framed (3840 px on the long side) or the whole sphere (7680 × 3840). The CPU stitcher renders it from freshly decoded full-res frames in about 4 s.
- **Hardware decode on any GPU vendor:** the first of Vulkan Video, VA-API and CUDA that decodes a test frame, else software. `--hwaccel` forces one.

Not done yet, roughly in priority order:

1. **Zero-copy decode.** Hand Vulkan Video frames to wgpu without the CPU round trip. This would allow full-res playback on the dev laptop.
2. **Native widget shapes.** native-theme-iced's per-widget styles (radii, padding, pressed and focus states); only the colours follow the OS today.
3. **Windows and macOS groundwork.** Add `d3d11va` and `videotoolbox` to `Hwaccel::pick`, and `cargo check` for both targets. Then hide the console window on Windows (the CLI commands still need one), and bundle ffmpeg or document installing it (mind its GPL builds). A macOS `.app` would need signing and notarisation.
4. **Seam, what's left.** Close objects at the seam beside the stick, such as the cap brim on 0025 at 19.5 s (the holder's head is always near the stick). Tried and dropped on 2026-10-10 (see Seam below): a per-frame optimal seam path, and a wider alignment search. Patch matching is tuned out, the stick is handled by the cut, and the sky by detail-only matching (see Seam below). Further gains need per-pixel optical flow. Seam analysis runs on the UI thread per frame; move it to the decode threads if it costs frames on slower machines.
5. **Timeline polish (branch `timeline`).** Done there: keyframes (with their ease), stabilisation per cut, thumbnails on the cut blocks, timeline zoom, `preview` of projects, GPU export, cuts (split, trim, move, delete, undo), project save/open and export from the player. Left:
   - Spherical metadata on equirect export.
6. **Version 0.3 (agreed with the user, 2026-10-10): crossfades and sound.**
   - Crossfades between cuts (picture and sound), in the player and the export.
   - Music: a track under the edit, with its level.
   - Each cut's volume, and mute.
   - Done: export as separate clips (`--clips`, `--handles`; the player's Clips checkbox) and the DaVinci Resolve format (`--format resolve`; the Resolve checkbox). Maybe later: an EDL/FCPXML beside the clips so an editor rebuilds the cut order, and handles in the player.
7. **Framing automation (a future release, agreed with the user 2026-10-10).** Asked for by the claudeVideoEdit editor; in this order:
   1. Preview bugs: `preview 0036 -s 440 -d 30 --every 0.5` fails while `-d 29.9` works (likely the tile count at the end boundary: 61 with both ends; get the exact error first), and "Broken pipe" from ffmpeg when we stop reading early (stop it cleanly or silence that).
   2. `yaw_world` on keyframes (the tool subtracts the smoothed heading in horizon mode, so a view stays on a landmark while the camera turns), and a **level** stabilisation mode: level horizon with the heading fixed in the world (unlike `lock`, which also keeps the starting tilt).
   3. Follow mode in keyframes, e.g. `{"t": 30, "follow": "stick", "offset_pitch": 30, "fov": 110}`, blending with fixed keyframes, plus a Follow picker in the player. The first source is `stick`: the operator is down the stick (rig +y) from the IMU attitude alone, so the third-person shot needs no vision and works straight below the camera.
   4. A `track` command for a colour, as a second follow source (`"follow": "track.csv"`). Track on sphere directions or in a perspective view around the last position, not on the equirect (it stretches at the poles).
   5. `scan --image`: opt-in columns from a low-res proxy stitch (open-sky fraction, horizon openness; maybe a "drop below" estimate). Opt-in because it decodes (~2 min on 0036) while plain `scan` reads only telemetry.
   Not chosen: person detection (needs an ONNX model; heavy dependency, YOLO is AGPL). Update the highlights skill with each step.
8. **Colour: D-Log M and a 10-bit pipeline (a later release, agreed with the user 2026-10-10; they will film D-Log M sample footage).** Together, because log footage needs the bits.
   - Today (0.2.6) a D-Log clip should load and export, flat and untransformed, but only in 8 bits: GPU export uploads NV12, CPU export decodes rgb24, and `--format resolve` writes 8-bit DNxHR HQ. The log curve survives for grading in Resolve; the precision doesn't, so heavy grades may band. Exposure matching assumes display-space colour and may even the seam less well on log (unmeasured).
   - Plan: P010 textures and a 16-bit render target; the Resolve format as 10-bit DNxHR HQX (`dnxhr_hqx`, `yuv422p10le`), left in log for grading; read the colour mode so the player and MP4 export apply a D-Log M → Rec.709 LUT, and tag the output's colour correctly.
   - Needs a D-Log M sample clip first: all 35 Kyrkja clips are normal colour, tagged BT.709 10-bit, as DJI may also tag D-Log.
   - DJI records the profile in `StreamMeta.color_mode` (`ColorModeType`: D_LOG = 2, HLG = 9, …). telemetry-parser's Metadata tag only serialises the clip meta, so reading it means decoding StreamMeta ourselves.

## Commands

```sh
cargo build --release
B=target/release/rosmo360
$B play [clip.OSV]                                    # player (see `$B play --help` for keys)
$B stitch clip.OSV -t 10 -w 2048 --stabilize horizon -o f.png
$B render clip.OSV -s 48 -d 12 -w 1280 --stabilize horizon -o out.mp4
$B rig clip.OSV > rig.json;  $B imu clip.OSV > imu.csv
$B seam clip.OSV -s 10 -d 3 > seam.csv                 # seam alignment quality (summary on stderr)
$B seam clip.OSV -s 7.37 -d 0.1 --dump DIR             # strips, differences and shift field as images
$B export clip.OSV --reframe r.json -o hl.mp4          # keyframed framing (--view Y,P[,R[,FOV]] for a fixed one; --draft from the proxy)
$B preview clip.OSV -o sheet.png                       # 24 stitched moments on one image (--every N, --reframe r.json)
$B scan clip.OSV > scan.csv;  $B info clip.OSV
```

Debug flags on `stitch`/`render`/`export`/`preview`:

- `--only 0|1`: render a single lens, to check overlap.
- `--variant N`: use an alternative calibration.
- `--no-rs`: disable rolling-shutter correction.
- `--no-exposure`, `--no-align`, `--no-cut`: disable the seam corrections, to compare.
- `--imu-axes`, `--world-z-down`: override the IMU mapping (only for other bodies).

Player keys:

- Drag: look. Right-drag or Q/E: roll. ↑/↓: pitch. Shift+←/→: yaw.
- Scroll: zoom.
- Space: play/pause. ←/→: seek ±5 s. `,` `.`: frame step.
- S: stabilisation mode. R: rolling shutter. P: projection. M: mute.
- Ctrl+O: open. Seek bar: drag to scrub.
- 0: reset view. F: fullscreen.
- Ctrl+O / Add clip…: append clips (several at once) as cuts. C: split the cut at the playhead. Drag a cut's edge to trim it, drag the cut to move it. Shift+Del: delete the selected cut. Ctrl+Z / Ctrl+Shift+Z: undo / redo. Ctrl+S / Ctrl+Shift+S / Ctrl+Shift+O: save / save as / open a project (`project::Project` JSON, the file `export` reads; clips under the project's folder are stored relative). `play edit.json` opens one. Closing the window or opening another project with unsaved changes asks Save / Don't save / Cancel (rfd message dialog; `exit_on_close_request` is off and `window::close_requests` drives it). Export… renders the edit with the CLI's `export` on a thread (progress and Cancel in the top bar), at the picked resolution, in the aspect lock's shape (16:9 when free), with the player's stabilisation and rolling-shutter settings.
- Timeline zoom: scroll on the timeline (around the pointer), Ctrl+= / Ctrl+−, or the buttons and slider in the transport row; Ctrl+0 fits the whole edit. Shift+scroll or a sideways scroll pans, as does the scrollbar under the seek bar (shown when zoomed). Closest zoom: 2 s across. While playing, and after key seeks, it pages to keep the playhead in sight. The app holds the zoom and start (`timeline_window` clamps them); `timeline::Timeline` gets the window as `start`/`span`.
- K: keyframe the view at the playhead (updates one there). `[` `]`: previous/next keyframe. Del: delete it. J or the Ease picker: how it moves on to the next (smooth ◆, linear ●, hold ■; the timeline draws that shape). On the timeline: click a diamond to jump, drag it to retime.
- While following keyframes (after play, seek or a keyframe edit) the view comes from them; looking around by hand takes over from that view until the next play or seek.

Runtime dependencies: `ffmpeg` and `ffprobe` CLIs, and ALSA (libasound) for sound on Linux; PipeWire and PulseAudio serve it through their ALSA plugins.

## Layout

- `src/main.rs`: CLI. `Pipeline` holds the shared setup: rig, lenses, stabiliser, fps.
- `src/stitch.rs`: CPU reference stitcher. It is the ground truth the shader must match.
  - Lens projection: ThetaPoly with 5 k terms plus tangential.
  - Rasterised validity masks.
  - Blend weights.
  - Rolling-shutter re-aim.
- `src/stab.rs`: `Stabilizer::rig_from_view(t_ms, readout_ms, steps)`, and `pose` (heading, tilt) for `scan`. Defines the `Mode` and `ImuAxes` enums.
- `src/view.rs`: `View` (yaw/pitch/roll/fov in degrees, projection), `Reframe` (keyframes, JSON with carry-over of missing fields) and `Camera` (what gets drawn). Shared by the player and `export`/`preview`. `Camera::ray` mirrors the shader's `screen_ray`; change them together.
- `src/project.rs`: `Project` (aspect, cuts) and `Cut` (input, in, out, its own `Keyframes` in clip time, and an optional `stabilize` that wins over `--stabilize`; the player sets it on every cut, from the picker or S while that cut plays; and an optional `note`, which the timeline shows after the clip name when there's room, for whoever assembled the edit to say what the cut is for). Keyframes stay pinned to the footage, so moving or trimming a cut keeps its camera moves; those outside in/out are kept for when it is extended again.
- `src/sheet.rs`: contact sheets for `preview`, with a tiny built-in bitmap font for the time labels.
- `src/ring.rs`: the seam ring: azimuth φ around lens 0's axis and angle α from it.
- `src/exposure.rs`: per-frame exposure matching (`Seam::measure`, `Seam::gain`).
- `src/align.rs`: per-frame seam alignment (`Aligner::measure`, `Aligner::warp`).
- The shader's `exposure_gain` and `align_warp` mirror `Seam::gain` and `Aligner::warp`; change them together.
- `src/imu.rs`: reads telemetry-parser's Quaternion/Gyro/Accel tags.
- `src/ffmpeg.rs`: RGB decode/encode through the ffmpeg CLI (used by `stitch`/`render`/photo save, hardware-decoded when possible), plus `Hwaccel` probing and the player's NV12 decode command.
- `src/player/`:
  - `mod.rs`: iced app: clip loading, widgets, keys, and the `Sphere` shader widget (drag/scroll to look). The open clip is a `project::Cut` holding its keyframes; `follow` says whether the view comes from them or from free look.
  - `timeline.rs`: canvas timeline. The top row shows the cuts as blocks with thumbnails (select, move, trim at the edges); the bottom row is the seek bar with the keyframe diamonds (click to jump, drag to retime). Two canvases in a `stack`: thumbnails underneath, everything else on top, because within one layer iced draws images over shapes (a `with_clip` region or a second geometry doesn't change that).
  - `thumbs.rs`: timeline thumbnails, the view straight ahead (horizon, 100° FOV) stitched on the CPU without seam corrections from the proxy's keyframes only (`ffmpeg::key_frames`, `-skip_frame nokey`; times from `showinfo`). A thread per clip when it loads, sent one at a time through a `Task::run` stream; none without a proxy. 0037 takes ~1 s, 0036 (13.5 min, 890 MB proxy) ~16 s, mostly reading the file.
  - `edit.rs`: the edit (cuts in order) with undo/redo snapshots, split/trim/move, and edit-time ↔ (cut, clip time). The app keeps a library of loaded `Media` and makes a fresh `Playback` when the playhead crosses into another clip's cut; `tick` plays on into the next cut at an out point.
  - `playback.rs`: playback clock, streams and full-res stills.
  - `gpu.rs`: iced shader `Primitive`/`Pipeline`: uniforms and NV12 textures, uploaded once per frame id. `Pipeline::create`/`prepare_frame`/`draw` don't need iced; `SeamFix::follow` (= `SeamSample::new` + `apply`) is the per-frame seam measure shared by playback and export.
  - `render.rs`: GPU export. `Gpu` is a wgpu device of its own rendering into an off-screen RGBA texture, two frames in flight (one renders while the other is read back); `encode` mirrors `crate::encode` (same frame times, attitude, rolling-shutter steps, last-frame padding). `crate::export` uses it unless `--cpu`, `--only` or no GPU (a software rasteriser doesn't count).
  - `shader.wgsl`: GPU port of `stitch.rs`.
  - `source.rs`: NV12 decode threads (LRF proxy / OSV) and full-res stills.
  - `audio.rs`: ffmpeg → f32 PCM → cpal output; frames played minus device latency give the clock.
  - `settings.rs`: settings remembered between runs (stabilisation, playback source, rolling shutter, full res when paused, volume/mute, export resolution, aspect lock) in `~/.config/rosmo360/settings.json` (XDG; `%APPDATA%` on Windows, Application Support on macOS). `App::update` saves them whenever a message changes them; `play --stabilize/--osv-size` override.
  - `export.rs`: PNG save of the paused frame.

## Releasing

`packaging/build-release.sh` writes the GitHub release files into `dist/`: `rosmo360-x86_64-linux.tar.gz` (binary, `install.sh`, menu entry, icons), `rOsmo360-x86_64.AppImage`, `install.sh` alone and `SHA256SUMS`. The names carry no version, so `releases/latest/download/<name>` (which `install.sh` downloads when run on its own) stays valid.

- It builds in an Ubuntu 22.04 container (`packaging/Dockerfile`): built on CachyOS the binary needs glibc 2.44 and won't start on Ubuntu 24.04 or Debian 12; this one needs 2.35. Checked on clean ubuntu:22.04 and debian:12 containers.
- The binary links only libasound, libgcc_s, libm and libc; iced loads Vulkan, Wayland and X11 at run time. The AppImage bundles nothing else: ffmpeg comes from the system, because a bundled (static) ffmpeg would lack the VA-API, Vulkan and CUDA decoders full-res stills and export depend on, and a bundled libasound would miss the system's PipeWire/Pulse ALSA plugins.
- `AppRun` opens the player when given nothing, a clip or a project, and otherwise passes the command line through.
- The icon is `packaging/rosmo360.svg` (`rosmo360.png` is rendered from it with `rsvg-convert -w 256`); the desktop file is `rOsmo360.desktop`, matching the app id so Wayland compositors find the icon.

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
- Tuning with `rosmo360 seam` (mismatch = mean lens disagreement across the blend band; jitter = mean per-frame change of the shift):
  - A temporal term (cost per degree from last frame's shift, 0.1) plus a stronger distance prior (0.06) cut eased jitter ~4× (0037: 0.224° → 0.054°/frame) with equal or lower mismatch.
  - Bigger patches lower mismatch slightly but double jitter. Finer cells (256 × 13) change mismatch < 0.5%.
  - A constant shift along the seam (φ) only makes mismatch worse, so the calibration lines up along the seam; the search stays 1-D.
- The selfie stick runs down rig +y (strip column 511 in `seam --dump`). There DJI's masks cut a hump out of each lens, so the overlap shrinks to ~86–94°. Lens 0 sees the arm running into its mask while lens 1 sees only the fist: different content, not a shift, so no alignment setting helps. The `seam` report's stick-zone score (±17° around the stick) showed it. A wider search (±9°), a capped distance prior, and shorter, wider or less complete patches all left it flat or worse.
- Matching runs on detail only (each strip minus its local mean, radius 4), and a shift must beat no shift by 0.05. On raw brightness, smooth sky gradients "matched" at large shifts and warped the sky along the seam into faint streaks. On 0043 at 8.81 s, alignment changed 4.3% of the sky before and 0.5% after. Eased jitter fell up to 5× (0037: 0.076° → 0.011°), for 2–3% more mismatch. Radius 2 calmed the sky more but hurt the stick zone; radius 8 barely helped. The cut was ruled out as the cause: it changed only 0.04% of that sky.
- The disagreement cut (`stitch::cut_share`, shader `cut_share`) handles it instead: from a 0.06 to 0.18 RGB difference it sharpens the blend to a ±0.08 step at the seam's middle. On 0043 it made the see-through fist solid, sharpened close rock and left the sky seam unchanged (exposure matching is why: the first try, before it, drew lines in the sky). It changes ~1.4% of the sphere.
- The cap brim cut off at the seam on 0025 at 19.5 s sits in the stick zone (strip columns 500–612, the stick at 511). There the masks leave an overlap of only 4.4–5.8° (17–19° elsewhere), while the brim's parallax between the lenses is ~6°. So the brim tip falls where only one lens has picture, and that lens sees rocks there; no cut placement inside the overlap can bring it back.
  - An optimal seam path (a DP over the aligned strips' disagreement, cut pivoting on it instead of 90°) chose ~88° and changed nothing visible. Even with a full overlap, a disagreement-minimising seam tends to delete a ghosted foreground rather than keep it.
  - A wider alignment search (±10°) with half the distance prior also made no clear difference.
  - DJI Studio on the same frame (user's screenshot, 2026-10-11): only a little better. The brim tip is mostly solid, with a soft edge and slightly smeared rocks beside it, the mark of its flow-based seam refinement. What's left is per-pixel flow that warps one lens into the other's exclusive area (big, for a small gain), or masks tighter than DJI's where the hump is wider than the stick (unexplored).
- Each lens's blend weight fades out over 2° inside its validity mask (`stitch::feather`, an exact distance transform at load; the shader reads it from the mask texture). Beside the stick DJI's masks cut into the ±6.84° blend band, so a lens used to stop dead mid-blend; where the lenses saw the sky differently that step was a line. On 0043 at 5.94 s (sun glare in lens 0: +58/+34/+9 RGB over lens 1 high in the overlap) it ran up the sky from the hand; feathered, it's gone, 0.1% of the sphere changed (all along mask edges), `seam` scores are unchanged and GPU export still matches the CPU at 41.7 dB. Glare itself is additive, which the per-bin gains only spread out (fading over 60–90°), so a broad gradient stays.
- The cut only acts where both lenses see detail across the blend band (`Field::cut`, per azimuth bin: the smaller of the lenses' mean |detail|, fading in from 0.008 to 0.02). Sun glare that one lens catches and the other doesn't is smooth (0037 at 34.6 s: 0.002–0.006), and the ungated cut drew a hard edge through the halo there. Hand and rock bins score 0.02–0.15, so 0043's stick frame rendered identically before and after.

### Hardware decode through the ffmpeg CLI

- Use `-hwaccel X -hwaccel_output_format X` then `hwdownload,format=F` with exactly one format: `p010le` for the 10-bit OSV, `nv12` for the 8-bit LRF. Listing both fails.
- That chain fails loudly when the GPU can't decode, because `hwdownload` rejects software frames; plain `-hwaccel` silently falls back. `Hwaccel::pick` relies on this.
- Convert to NV12 and scale on the CPU. GPU filters are not portable: on NVIDIA, `scale_vaapi` (nvidia-vaapi-driver) and `scale_vulkan` to NV12 fail, and `scale_cuda` was slower. The cost is decode plus download, not conversion.
- RTX 3090: Vulkan decodes one lens at 3.2× real time (CUDA 3.7×). Both lenses at full 3840² run at about 1.1× (Vulkan) to 1.3× (CUDA) real time, which is enough for full-res playback.

- Starting an ffmpeg decode costs ~1 s for Vulkan setup and ~0.5 s for CUDA, before any frame. For one proxy (H.264) frame, software takes 0.15 s, so the proxy always decodes in software: 4× real time on one thread, 8× on four. That made scrubbing responsive. One OSV (HEVC 10-bit 3840²) frame still needs the GPU: 1.5 s on Vulkan, 8 s in software.

### Decode performance on the dev laptop (Intel UHD 620)

- VA-API HEVC decode alone runs at about 3× real time per lens.
- The bottleneck is `scale_vaapi` + `hwdownload` + pipe of two 3840² tracks: 0.55× real time at full res, and only about 1.07× at 1920².
- So playback uses the proxy, and real full-res playback needs zero-copy.

### Framing and export

- Perspective and little planet are one azimuthal family: the ray is at θ from the view axis where s = sin θ / (d + cos θ), s being the distance from centre in units of tan(fov/2). d = 0 is perspective and d = 1 stereographic (the player's original planet exactly), so keyframes morph between them by blending d. The shader gets d in `screen.w` (negative = equirect).
- In horizon mode, equirect longitude/latitude = view yaw/pitch, which is why preview tiles map directly to `--view` angles. Lock mode holds the clip's first orientation including its tilt (0037 starts tilted ~49°), so it is level only if the clip started level.
- The Osmo 360 records only the fused attitude (no raw gyro or accelerometer tags), so `scan` derives turning and shake from attitude differences every 20 ms. On 0037 it shows the known 87° pan at 15–18 s.
- The stitcher reads the LRF proxy by splitting it into per-lens 1024² frames; `Lens::sample` scales picture coordinates to whatever frame size it gets.
- Speed on the RTX 3090 desktop (i7-10700K): CPU export ~5 fps at 1080×1920, `--draft` ~8 fps; a 24-tile preview from the proxy ~15 s.
- GPU export (`render.rs`) runs ~21 fps at 1080p full quality, and matches the CPU at 40-41 dB PSNR (the difference is the 8-bit NV12 decode against ffmpeg's RGB, and x264). A 6 s, 3-cut project: 12 s against 40 s (each cut restarts a decoder, ~1 s). How it got there, on 0037:
  - Done in turn, a frame took 22 ms of seam measure (strips 14, matching 17 under load), 14 ms texture upload (two 3840² NV12), 10 ms GPU and 6 ms RGBA→RGB: 11 fps. Overlapping them on threads (decode → seam sample → seam match + attitude → render, with two readback buffers) gave 16 fps.
  - Then the decode limited it: Vulkan decodes both 3840² lenses at only ~1× real time, because the cost is downloading 10-bit P010. CUDA with `scale_cuda=format=nv12` converts on the GPU and downloads half the bytes: 1.9× real time alone, and the export went to 21 fps. `Hwaccel::works` probes CUDA once a run; export uses it for the full-res OSV when it works (`ffmpeg::nv12`).
  - Now the whole CPU is busy (16 threads). The x264 preset barely matters (veryfast 22 fps), nor does NVENC (24 fps): x264 only takes cycles nothing else wants. CachyOS also nices ffmpeg below us. Next gains would be buffer reuse in the decode pump (allocation and page faults: ~1 s of system time per second) or the seam measure on the GPU.

### iced and wgpu

- iced 0.14 is on wgpu 27. Use its re-export `iced::wgpu`; never add a direct `wgpu` dependency, or the versions split.
- iced picks the surface format. The shader outputs display-encoded BT.709 and linearises only when the target is sRGB (`misc.y`).
- The widget draws into a sub-rectangle, so the shader subtracts its framebuffer origin (`misc.zw`) from `@builtin(position)`.

## How to verify changes

- **Stitch geometry:** run `stitch --only 0` and `--only 1`, then crop around longitude ±90°. Content must match across the overlap. Black areas are DJI's validity masks.
- **Seam alignment:** `seam` on 0043, 0037 and 0026; compare mismatch (raw/eased vs none) and eased jitter before and after.
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
