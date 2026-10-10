# rOsmo360

A 360° video player for **DJI Osmo 360** footage, written in Rust. It plays the camera's `.OSV` files directly: it stitches the two fisheye lenses, stabilises with the camera's gyro data and lets you look around freely, without going through DJI Studio.

![rOsmo360 playing a clip from a mountain hike, looking out over a lake and glaciers](docs/screenshot.png)

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Why

I vibed this tool primarily for Linux, since I couldn't find anything close to DJI Studio there. The goal of rOsmo360 is to bring most of DJI Studio's playback and some limited editing functionality to Linux users. It isn't Linux-exclusive, though: Windows and macOS support is in the pipeline.

## Features

- **Stitching** with DJI's own calibration, read from each clip. Per frame it also:
  - evens out the two lenses' exposure along the seam;
  - shifts the lenses to line up close subjects, such as the hand on a selfie stick, so they don't ghost.
- **Stabilisation** from the camera's built-in 1 kHz motion data, with rolling-shutter correction:
  - **Horizon**: a level horizon that follows where you point the camera.
  - **World lock**: the view stays fixed in the world.
- **Free look**: drag to look around, scroll to zoom, and roll the view.
- **Projections**: little planet (the default, like DJI Studio), perspective and flat equirectangular.
- **Smooth playback** from the low-resolution proxy (`.LRF`), switching to the full-resolution frame when paused. On a fast GPU it can stream the full-resolution `.OSV` directly.
- **Hardware decoding** on any GPU vendor: Vulkan Video, VA-API or CUDA, with a software fallback.
- **Sound**, in sync with the picture, with mute and volume.
- **Save photos** of a paused frame: the view as framed, at 3840 px on the long side, or the whole 360° sphere at 7680 × 3840.
- **Aspect lock** (16:9, 9:16, 4:3, 1:1, 2:1) for framing shots; saved views take the same shape.
- **Looks native**: follows your desktop's colour theme (KDE, GNOME, Windows, macOS), including live changes.

Command-line tools can also stitch single frames or whole clips to equirectangular images and MP4 video.

## Requirements

- A DJI Osmo 360 clip (`.OSV`). Keep the `.LRF` proxy file next to it for the smoothest playback.
- `ffmpeg` and `ffprobe` on your `PATH`.
- A GPU with Vulkan support (Linux) and up-to-date drivers.
- On Linux, ALSA for sound (`alsa-lib`, or `libasound2-dev` on Debian/Ubuntu). PipeWire and PulseAudio work through it.
- To build: Rust 1.88 or newer.

Developed and tested on Linux (KDE Plasma, NVIDIA RTX 3090). Windows and macOS builds are planned but not yet tested.

## Building

```sh
cargo build --release
```

The program is `target/release/rosmo360`.

## Usage

```sh
rosmo360 play                 # opens the player; use Open… or Ctrl+O
rosmo360 play clip.OSV        # opens a clip straight away
```

In the player:

| Action | How |
|---|---|
| Look around | Drag (right-drag or Q/E rolls) |
| Zoom | Scroll, or `+` / `-` |
| Play / pause | Space |
| Seek | Drag the seek bar, or ← / → for 5 s |
| Step one frame (paused) | `,` and `.` |
| Stabilisation / projection | S / P |
| Rolling-shutter correction | R |
| Mute | M |
| Reset view / fullscreen | 0 / F |

Command-line tools:

```sh
rosmo360 stitch clip.OSV -t 10 -w 4096 --stabilize horizon -o frame.png   # one frame to an equirect PNG
rosmo360 render clip.OSV -s 48 -d 12 -w 3840 --stabilize horizon -o out.mp4   # a span to an equirect MP4
```

`rosmo360 --help` lists everything, including `rig` (calibration as JSON), `imu` (motion data as CSV) and `seam` (seam quality report).

## Roadmap

- Smooth full-resolution playback on modest hardware (zero-copy decoding).
- Windows and macOS builds.
- Video export of a framed view.
- D-Log M colour support.

## License

rOsmo360 is licensed under either of

- the [MIT License](LICENSE-MIT), or
- the [Apache License, Version 2.0](LICENSE-APACHE),

at your option. Unless you state otherwise, any contribution you submit for inclusion in rOsmo360 is dual-licensed as above, without any additional terms or conditions.

The Vibing with Ravelund logo ([`vibewithravelund-lowres-corrected.jpg`](vibewithravelund-lowres-corrected.jpg)) and the screenshot ([`docs/screenshot.png`](docs/screenshot.png)) are not covered by these licences; all rights reserved.

## Support

rOsmo360 is free, and it always will be. If you find it useful and feel like buying me a coffee, you can do that through [PayPal](https://www.paypal.com/donate/?hosted_button_id=78BF2LSB8FGR8). Thank you! ☕

## Credits

Made by **Vibing with Ravelund**, 2026. rOsmo360 was built through AI-assisted ("vibe") coding with Claude.

It stands on the shoulders of:

- [telemetry-parser](https://github.com/AdrianEddy/telemetry-parser) by Adrian Eddy, which reads the camera's calibration and motion data;
- [iced](https://iced.rs) (GUI), [wgpu](https://wgpu.rs) (GPU rendering), [cpal](https://github.com/RustAudio/cpal) (sound), [native-theme](https://github.com/tiborgats/native-theme) (desktop theming) and [rfd](https://github.com/PolyMeilex/rfd) (file dialogs);
- [FFmpeg](https://ffmpeg.org), used as an external program for decoding.

rOsmo360 is an independent project. It is not affiliated with or endorsed by DJI. DJI, Osmo and DJI Studio are trademarks of SZ DJI Technology Co., Ltd.
