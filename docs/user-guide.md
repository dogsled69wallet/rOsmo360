# rOsmo360 user guide

rOsmo360 plays and edits 360° video from the **DJI Osmo 360**. It reads the camera's `.OSV` files directly, stitches the two lenses, keeps the horizon level, and lets you look around, cut clips together, move the camera's view with keyframes and export a finished video.

Part 1 walks through the app itself; everything there is done with the mouse and keyboard. Part 2 covers the command line, for batches, scripts and a few extra tools. For building rOsmo360 from source, see the [README](../README.md).

**Contents**

Part 1: Using rOsmo360

1. [Getting started](#1-getting-started)
2. [The window](#2-the-window)
3. [Watching a clip](#3-watching-a-clip)
4. [Projections](#4-projections)
5. [Stabilisation](#5-stabilisation)
6. [Building an edit on the timeline](#6-building-an-edit-on-the-timeline)
7. [Keyframes: moving the camera](#7-keyframes-moving-the-camera)
8. [Framing: the aspect lock](#8-framing-the-aspect-lock)
9. [Projects](#9-projects)
10. [Exporting video](#10-exporting-video)
11. [Saving photos](#11-saving-photos)
12. [Keyboard shortcuts](#12-keyboard-shortcuts)
13. [Tips and troubleshooting](#13-tips-and-troubleshooting)

Part 2: Advanced

14. [The command line](#14-the-command-line)
15. [The clip cache](#15-the-clip-cache)

---

# Part 1: Using rOsmo360

## 1. Getting started

### Installing

For the install script and the AppImage, install `ffmpeg` from your distribution first (`sudo apt install ffmpeg`, `sudo dnf install ffmpeg` or `sudo pacman -S ffmpeg`). Then take one of the ready-made builds from the [releases page](https://github.com/ravelund/rOsmo360/releases/latest). They run on x86_64 Linux: Ubuntu 22.04, Debian 12, Fedora 36 and newer.

- **Install script**: puts rOsmo360 in your application menu. Paste this into a terminal:

  ```sh
  curl -fsSL https://github.com/ravelund/rOsmo360/releases/latest/download/install.sh | sh
  ```

  Run it again to update. Adding `--uninstall` removes it; your settings stay.
- **AppImage**: download `rOsmo360-x86_64.AppImage`, make it executable (in your file manager's properties, or `chmod +x rOsmo360-x86_64.AppImage`) and double-click it.
- **Flatpak**: download `rOsmo360-x86_64.flatpak` and open it with your software centre (Discover, GNOME Software), or install it in a terminal with `flatpak install --user rOsmo360-x86_64.flatpak`. It brings everything it needs, ffmpeg included, so you can skip installing ffmpeg. Its exports are a little slower on NVIDIA cards.

### What you need

- Clips from the Osmo 360 (`.OSV`). Copy the `.LRF` file that the camera records next to each clip as well. It's a small preview version, and rOsmo360 uses it for smooth playback, scrubbing and thumbnails.
- A GPU with up-to-date drivers (Vulkan on Linux). It decodes the video, draws the view and renders exports.

### Opening clips

Start **rOsmo360** from your application menu.

![The start screen: the Vibing with Ravelund logo, and the buttons Open clips… and Open project…](guide/start.jpg)

**Open clips…** picks one or more clips; **Open project…** opens a saved edit. A bar shows how far loading has got. The first time, a clip takes a while to open, longest from a network drive or a spinning disk, because rOsmo360 reads the camera's motion data from all through the file. It remembers what it read, so the same clip opens almost at once after that, in any project.

The first clip starts playing as soon as it has loaded; the others load behind it.

## 2. The window

![The rOsmo360 editor: a view over a cairn on a ridge and a lake below glaciers, with the timeline of four cuts underneath](guide/editor.jpg)

From top to bottom:

1. **The top bar**: projects, clips, the export, and what is playing.

   ![The top bar: icons for new project, add clips, open project, save and save as; Aspect 16:9; the export icon, 1080p, the Clips and Resolve checkboxes, the clip's name, and two dimmed status icons](guide/top-bar.png)

   The icons, from the left: **new project** (a page with a plus), **add clips** (a film strip with a plus), **open a project** (a folder), **save** (a diskette; dimmed when there's nothing new to save) and **save as** (a diskette with a pencil). Then the **Aspect** of the picture, and **export** (an arrow out of a tray) with its resolution and options. While an export or a colour track runs, its progress shows here, with a crossed circle to stop it.

   On the right, two dimmed icons say what you're seeing (a film strip for the quick preview version, a picture for the full video, an hourglass while a sharp paused frame loads) and how the video is decoded (a lightning bolt when the GPU does it). Messages, such as where an export went, appear beside them.

2. **The view**, where you look around.

3. **The transport row**: play, photos, the time, keyframe and cut tools, timeline zoom, and sound.

   ![The transport row: play, save view, save 360°, the time, add and delete keyframe, the Ease and Follow pickers, the eyedropper, split, delete cut, undo, redo, the timeline zoom, mute and volume](guide/transport.png)

4. **The timeline**: the cuts of your edit, with thumbnails, and below them the seek bar with the keyframes.

   ![The timeline: four cuts with thumbnails and labels such as "0045 · 12.0 s" and "0026 · 10.0 s · world lock", and the seek bar with keyframe marks](guide/timeline.png)

5. **The settings row**: stabilisation, projection, playback source, field of view, rolling-shutter correction, full resolution when paused, reset view (a turn-back arrow) and fullscreen (corners).

   ![The settings row: Stabilisation Horizon, Projection Perspective, Playback source Proxy (LRF), the FOV slider, Rolling shutter, Full res when paused, and the reset-view and fullscreen icons](guide/settings.png)

**Hover over any button** to see what it does and its keyboard shortcut.

## 3. Watching a clip

| To | Do |
|---|---|
| Play or pause | Space, or ▶ |
| Seek | Click or drag on the seek bar; ← / → jump 5 seconds |
| Step one frame (paused) | `,` and `.` |
| Look around | Drag in the view, or use ↑ / ↓ (pitch) and Shift+← / → (yaw) |
| Roll the view | Right-drag, or Q / E |
| Zoom | Scroll in the view, `+` / `-`, or the **FOV** slider |
| Go back to the starting view | 0 or Home, or the turn-back arrow |
| Fullscreen | F, or the corners icon; Esc leaves it |
| Mute | M, or the speaker icon; the slider sets the volume |

**Playback source.** Clips play from the `.LRF` proxy by default. It decodes quickly, so playback and scrubbing stay smooth on most computers. On a fast GPU you can play the `.OSV` itself, scaled to 1920 px or at its full 3840 px. If there's no `.LRF`, the `.OSV` is used.

**Full res when paused.** When this is on, pausing swaps in a sharp full-resolution frame after a moment. Turn it off to keep the paused picture at the playback source's quality. The picture then doesn't jump in sharpness while you set keyframes. Exports always use the full resolution either way.

## 4. Projections

Press **P**, or use the **Projection** picker, to switch between three ways of showing the sphere.

**Perspective** is a normal camera view: straight lines stay straight. It's the one for most shots.

![A perspective view of a cairn on a rocky ridge, with mountains and a lake](guide/perspective.jpg)

**Little planet** wraps the whole sphere into a circle. Looking down (the default) makes a tiny planet with the ground in the middle. Looking up turns it inside out, with the sky in the middle and the landscape around it:

![A little-planet view looking up: blue sky and the sun in the middle, ringed by snowy mountains and lakes](guide/little-planet.jpg)

**Equirect** shows the whole sphere flat, as a 2:1 map. It's useful for seeing everything that's around at once.

Perspective and little planet are one family. A keyframed move can morph smoothly from one into the other (see [keyframes](#7-keyframes-moving-the-camera)).

## 5. Stabilisation

The camera records its own motion about a thousand times a second, and rOsmo360 uses that to steady the picture.

- **Horizon** (the default) keeps the horizon level and follows where the camera points, smoothed. It looks like a steady camera operator.
- **Level** keeps the horizon level like Horizon, but holds the direction the camera pointed when the clip started, instead of following it. A mountain stays put while the camera turns, and the picture stays level even if the camera started tilted. Use it for a view that should stay on one thing while you turn around.
- **World lock** also holds the view still in the world, but keeps the clip's starting orientation exactly, including any tilt it started with.
- **Off** shows the picture as the camera saw it.

Press **S**, or use the **Stabilisation** picker. **Each cut keeps its own mode**: the picker changes the cut that's playing, and new cuts get the mode you used last. On the timeline, a cut that isn't on horizon says so in its label, for example "world lock".

**Rolling shutter** (R) corrects the slight wobble and lean caused by the sensor reading out line by line during fast turns. Leave it on.

## 6. Building an edit on the timeline

An *edit* is a row of *cuts*, each a stretch of one clip, played one after another. Every clip you open becomes a cut.

- **Add clips**: the film strip with a plus, or Ctrl+O; you can pick several at once. Each is added at the end.
- **Select a cut**: click it in the top row of the timeline.
- **Split**: put the playhead where you want the cut, then press **C** or ✂. Both halves keep the cut's keyframes, so the camera move carries on unchanged.
- **Trim**: drag a cut's left or right edge.
- **Move**: drag a cut sideways; a line shows where it will land.
- **Delete**: select it and press **Shift+Delete**, or 🗑.
- **Undo / redo**: Ctrl+Z and Ctrl+Shift+Z (or Ctrl+Y).

Playback runs through the cuts in order, straight on from one into the next, even when the next one comes from a different clip: rOsmo360 gets the next cut ready a few seconds ahead, so the change is smooth.

Each cut shows thumbnails of its footage, the view straight ahead. They fill in a moment after a clip loads; a long clip takes a few seconds the first time.

A cut's label shows its clip and length, its stabilisation when it differs, and its note when there's room. Notes come from the project file, so whoever put the edit together can say what each cut is for; the player shows them but doesn't edit them yet.

### Zooming the timeline

With a long edit, short cuts and close keyframes get narrow. Zoom in:

- **Scroll** on the timeline to zoom around the mouse pointer.
- **Ctrl+=** / **Ctrl+−**, or the magnifier icons and the slider between them, zoom around the playhead.
- **Ctrl+0**, or the arrows-to-both-ends icon, shows the whole edit again.

![The timeline zoomed in on the end of the first cut and the start of the second: thumbnails of the rock tower and the rock dome, the seek bar with two keyframe squares, and a scrollbar underneath](guide/timeline-zoomed.png)

When zoomed in, a scrollbar appears under the seek bar. Drag it, use Shift+scroll, or scroll sideways on a touchpad to move along. While playing, the timeline scrolls along to keep the playhead in sight.

## 7. Keyframes: moving the camera

A **keyframe** remembers a view at a moment of a cut: where it looks (yaw, pitch, roll), the zoom, and the projection. Between keyframes the view moves smoothly from one to the next. That's how you make pans, slow zooms and morphs into a little planet.

1. Go to the moment where the move should start, and frame the view.
2. Press **K**, or the diamond-with-a-plus icon. A mark appears on the seek bar.
3. Go to where the move should end, frame the new view, and press **K** again.
4. Play it back.

More:

- **Update a keyframe**: go to it and press K again with the new view.
- **Jump between keyframes**: `[` and `]`, or click a mark.
- **Retime a keyframe**: drag its mark along the seek bar.
- **Delete a keyframe**: go to it and press **Delete**, or the crossed-diamond icon.

**Following and looking.** After you play, seek or change a keyframe, the view follows the keyframes. As soon as you look around by hand, you take over from the view they showed, until the next play or seek. So you can always look around freely without changing anything; only K saves a view.

**Cuts without keyframes** hold the view the edit had last: the view at the end of the nearest cut before them that has keyframes. The export frames them the same way. If no earlier cut has keyframes, the player keeps whatever view you look at, but the export looks straight ahead, so press K in such a cut to keep its view.

Keyframes belong to their cut and stay pinned to the footage. Moving or trimming a cut keeps its camera moves on the same moments. Keyframes trimmed off the end are kept for when you extend the cut again.

### Ease: how a keyframe moves on

Each keyframe also says how the view travels from it to the *next* one. Choose it in the **Ease** picker while the playhead is on the keyframe, or press **J** to cycle through the three:

| Ease | Mark | From this keyframe to the next |
|---|---|---|
| **Smooth** | ◆ | Starts gently, moves, and slows into the next keyframe. The default; good for most moves. |
| **Linear** | ● | Moves at a steady speed. Good for an even pan, or for several keyframes in a row that should feel like one move. |
| **Hold** | ■ | Stays still, then jumps to the next keyframe's view when the playhead gets there. A cut to a new angle within one cut. |

![The playhead on a linear keyframe (a circle on the seek bar); the Ease picker shows Linear](guide/keyframe-ease.png)

Two common patterns:

- **Pause, then pan**: two keyframes with the same view (the first Smooth or Hold), then a third with the new view.
- **Cut between angles**: set a keyframe to Hold, and put the next keyframe, with the other angle, where the cut should happen.

### Follow: a third-person shot

A keyframe can **follow the selfie stick** instead of looking in a fixed direction. The stick points at whoever holds it, so the view keeps you in the picture as you walk, turn and move the stick, like a drone following from above and behind. rOsmo360 knows where the stick points from the camera's own motion data, so it keeps working even when the stick points straight down.

1. Frame the shot you want around yourself: for example tilt the view up a little from the stick so the trail ahead shows too, and widen it (FOV 100 or so).
2. Press **K**, then choose **Stick** in the **Follow** picker. The view stays as it is: the keyframe now keeps it as an offset from the stick. Its mark on the seek bar gets a ring.
3. Play. Keyframes you add after it follow too, until you set one back to **Fixed**. A move from a following keyframe to a fixed one, or back, eases over as usual.

To follow **someone or something else**, such as a friend in a red jacket, track it by its colour:

1. Pause where the subject is in view, at the keyframe that should start following it.
2. Click the **eyedropper** icon next to the Follow picker, or choose **Colour track** in the picker, and drag a box over the subject. A box over a good patch of the colour works better than a thin edge. Esc cancels.
3. rOsmo360 takes the box's colour and tracks it from there to the end of the cut (the top bar shows **Tracking** with its progress), saves the track next to the project, and makes the keyframe follow it with the subject in the middle. The top bar then says in how many moments it found the colour.

Pick something with a strong colour that nothing big nearby shares: sky, grass, rock and snow won't track. Small things that swing, like legs, follow less steadily than a jacket.

## 8. Framing: the aspect lock

The **Aspect** picker in the top bar sets the shape of the picture: Free, 16:9, 9:16, 4:3, 1:1 or 2:1. The view is letterboxed to that shape, so you frame exactly what will be exported. Use 9:16 for phone video such as Reels, Shorts or TikTok:

![The editor with the aspect lock on 9:16: a tall, narrow view of the cairn and the mountains behind it, with black bars either side](guide/vertical.jpg)

Exports and saved views take the same shape. With **Free**, exports are 16:9.

## 9. Projects

A project saves the edit: the cuts, their keyframes and stabilisation, and the aspect lock.

- **Save**: Ctrl+S, or the diskette. **Save as**: Ctrl+Shift+S, or the diskette with a pencil.
- **Open**: Ctrl+Shift+O, or the folder.
- **New**: Ctrl+N, or the page with a plus, starts an empty project without closing the window. Clips you had open stay loaded, so adding them again is instant.

The window title shows the project's name, with a dot (•) when there are unsaved changes. If you close the window, open another project or start a new one with unsaved changes, rOsmo360 asks whether to **Save**, **Don't save** or **Cancel**.

Clips in the project's folder, or in folders inside it, are remembered by relative path, so you can move the whole folder, or copy it to another computer, and the project still finds its clips. If a clip is missing when you open a project, its cuts are left out and you're told which.

A project starts playing as soon as its first cut's clip has loaded; the others load behind it, with the bar in the top bar. If playback reaches a cut whose clip isn't in yet, it waits there.

## 10. Exporting video

1. Pick the resolution next to the export icon (an arrow out of a tray): 720p, 1080p (the default, good for sharing online), 1440p or 2160p (4K). It's the short side of the picture, so 1080p is 1920 × 1080 for 16:9, or 1080 × 1920 for 9:16.
2. Set the shape with the **Aspect** picker.
3. Click the export icon and choose where to save the MP4.

The export renders the whole edit at full resolution, with the sound joined across the cuts. Each cut uses its own keyframes and stabilisation, and the export follows the rolling-shutter setting. Progress shows in the top bar, with a crossed circle to stop it; a cancelled export leaves no half-finished file.

Exports render on the GPU. On an RTX 3090 that's about 21 frames a second at 1080p, so a one-minute edit takes about a minute and a half. You can keep using the player while it runs.

Two checkboxes next to the resolution change what gets written:

- **Clips** writes each cut to its own file instead of one video, for finishing in another editor. Export then asks for a folder, and the files are named after the project: `My hike - 001.mp4`, `My hike - 002.mp4` and so on, in timeline order.
- **Resolve** writes DNxHR HQ video with uncompressed PCM sound in a `.mov`, instead of H.264 and AAC in an MP4. Use it for DaVinci Resolve: its free version on Linux can't read H.264, H.265 or AAC. The files are large, about 27 MB a second at 1080p (110 at 4K), as editing formats are.

Both are remembered, and they combine: Clips and Resolve together give one `.mov` per cut, ready to drop into Resolve.

## 11. Saving photos

Pause, then:

- **Save view** (the camera icon) saves what you see as a PNG, 3840 px on the long side, in the aspect lock's shape.
- **Save 360°** (the globe icon) saves the whole sphere as a 7680 × 3840 equirectangular PNG. Most photo sites and viewers that support 360° photos can show it.

Both are rendered from fresh full-resolution frames, which takes a few seconds.

## 12. Keyboard shortcuts

| Key | Action |
|---|---|
| Space | Play / pause |
| ← / → | Seek 5 seconds |
| `,` / `.` | Step one frame back / forward (paused) |
| ↑ / ↓ | Look up / down |
| Shift+← / → | Look left / right |
| Q / E | Roll left / right |
| `+` / `-` | Zoom in / out |
| 0 or Home | Reset the view |
| P | Next projection |
| S | Next stabilisation mode (for the playing cut) |
| R | Rolling-shutter correction on / off |
| M | Mute |
| F / Esc | Fullscreen / leave fullscreen |
| K | Add a keyframe here, or update the one here |
| J | Cycle the keyframe's ease: smooth, linear, hold |
| `[` / `]` | Previous / next keyframe |
| Delete | Delete the keyframe here |
| C | Split the cut at the playhead |
| Shift+Delete | Delete the selected cut |
| Ctrl+Z / Ctrl+Shift+Z (Ctrl+Y) | Undo / redo |
| Ctrl+= / Ctrl+− | Zoom the timeline in / out |
| Ctrl+0 | Show the whole edit on the timeline |
| Ctrl+O | Add clips |
| Ctrl+N | New project |
| Ctrl+S / Ctrl+Shift+S | Save the project / save as |
| Ctrl+Shift+O | Open a project |
| Esc | Cancel drawing a colour box |

With the mouse: drag in the view to look around, right-drag to roll, scroll to zoom. On the timeline, click to seek, click a keyframe mark to jump to it, drag a mark to retime it, drag a cut to move it or its edges to trim it, and scroll to zoom.

## 13. Tips and troubleshooting

- **Playback stutters.** Use the **Proxy (LRF)** source; it needs far less than the full `.OSV`. If a clip has no `.LRF`, copy it from the camera's card; DJI records one next to every clip. Footage on an SSD plays and scrubs more smoothly than on a spinning disk or a network drive.
- **Clips open slowly the first time.** That's rOsmo360 reading the camera's motion data; the second time the clip opens almost at once. To have a whole card ready before you start, see [the clip cache](#15-the-clip-cache).
- **The paused picture keeps changing sharpness.** Turn off **Full res when paused** while keyframing.
- **Something close to the camera looks doubled or cut at the edges.** That's the seam where the two lenses meet. rOsmo360 lines up close subjects and cuts cleanly where it can't, but a hand or face very close to the seam can still show it, most of all next to the selfie stick, where the two lenses overlap least. Turn the view so close subjects sit away from the sides of the camera.
- **"World lock" starts tilted.** It holds the clip's first orientation. Use **Level** instead: it holds the direction but keeps the horizon level.
- **No sound.** Check the speaker icon isn't showing muted (mute is remembered between runs). On Linux, sound goes through ALSA, which PipeWire and PulseAudio both provide.
- **Export says it's stitching on the CPU.** No usable GPU was found, so it falls back to the slower CPU stitcher. Update your GPU drivers. The result looks the same.
- **Your settings** (stabilisation, playback source, rolling shutter, full res when paused, volume and mute, export resolution and options, and aspect) are remembered between runs.

---

# Part 2: Advanced

## 14. The command line

Everything the player does can also be done from a terminal, which is handy for batches and scripts, and a few tools exist only there. `rosmo360 --help` and `rosmo360 <command> --help` list every option. If you use the AppImage, write `./rOsmo360-x86_64.AppImage` where these say `rosmo360`; with the Flatpak, `flatpak run io.github.ravelund.rOsmo360`. The Flatpak has a `/tmp` of its own, so give it output paths elsewhere.

**Open** the player with something loaded:

```sh
rosmo360 play                  # the start screen
rosmo360 play clip.OSV         # one clip
rosmo360 play a.OSV b.OSV      # several clips, one after another on the timeline
rosmo360 play edit.json        # a saved project
```

**Export** a project, or a clip with a fixed or keyframed view:

```sh
rosmo360 export edit.json -o edit.mp4                         # a project saved from the player
rosmo360 export clip.OSV -s 30 -d 8 --view 40,-10 -a 9:16 -o reel.mp4
rosmo360 export clip.OSV --reframe moves.json -r 2160p -o reel.mp4
rosmo360 export edit.json --draft -o draft.mp4                # quick and soft, from the proxies
rosmo360 export edit.json --clips --handles 1 -o clips/        # a file per cut, with 1 s extra either side
rosmo360 export edit.json --format resolve -o edit.mov         # for DaVinci Resolve on Linux
```

`--handles` adds footage before and after each clip, where the camera recorded it, so another editor has room for its own transitions.

**Preview** an edit or a clip as one image. For a project you get a row per cut, each tile framed as the export will frame it:

```sh
rosmo360 preview edit.json --every 2 -o check.png
rosmo360 preview clip.OSV -o sheet.png                        # 24 moments of a clip, the whole sphere
```

![A contact sheet of a four-cut project: rows of small framed views of a ridge, a rock tower, a boulder slope, a lake valley turning into an upward little planet, and a boulder field, each labelled with its cut number and time](guide/contact-sheet.jpg)

**Track a subject by colour**, the command-line version of the eyedropper (see [Follow](#follow-a-third-person-shot)):

```sh
rosmo360 track clip.OSV --colour "#c8302a" -s 40 -d 20 > friend.csv
```

Then give a keyframe `"follow": "friend.csv"` in the project file. Read the colour off a preview or a saved photo. If something else nearby has the same colour, add `--near YAW,PITCH` with where the subject is at the start, as shown on a `--stabilize level` preview.

**Find good moments**: `rosmo360 scan clip.OSV > scan.csv` gives, per second, where the camera points, how fast it turns or shakes, and how loud it is. Add `--image` for three columns from the picture: how much of the sky is open (`sky`), how high the skyline is around you (`skyline`, in degrees: near 0 on a summit, higher in a valley) and in how many directions you can see out to the horizon (`open`). Sort by `open` to find the panoramas. It needs the `.LRF` proxy and takes a little longer: about 20 s for a 13-minute clip. `rosmo360 info clip.OSV` prints the clip's length, frame rate and recording time.

**Stitch** whole clips or frames to equirectangular video or images, for other 360° tools:

```sh
rosmo360 render clip.OSV -s 48 -d 12 -w 3840 --stabilize horizon -o out.mp4
rosmo360 stitch clip.OSV -t 10 -w 4096 --stabilize horizon -o frame.png
```

Projects are small JSON files you can also write or change by hand; the [README](../README.md#usage) describes the project and reframe formats, including cut notes (`"note": "summit arrival"`), per-cut stabilisation and following keyframes.

## 15. The clip cache

What rOsmo360 reads from a clip the first time, the calibration, the motion data and the timeline's thumbnails, is kept in a cache, so the next time the clip opens in a fraction of a second, in any project and from the command line too. (One 7-minute clip on a network share: 46 s the first time, 0.4 s after.) It recognises a clip by its contents, so moving or renaming footage doesn't matter, and a changed file is read afresh.

```sh
rosmo360 cache                                  # where it is, how many clips, how big
rosmo360 cache add ~/Videos/Hike                # cache every clip in a folder ahead of time
rosmo360 cache add ~/Videos/Hike --force        # read them all afresh, even those cached
rosmo360 cache move /mnt/media/rosmo360-cache   # keep it elsewhere from now on
rosmo360 cache move --default                   # back to the usual place
rosmo360 cache clear                            # empty it
rosmo360 cache clear --older-than 90            # only what no clip has used for 90 days
```

- **Where**: `~/.cache/rosmo360` on Linux (in the Flatpak, `~/.var/app/io.github.ravelund.rOsmo360/cache/rosmo360`; `%LOCALAPPDATA%\rosmo360` on Windows), about 1.5 MB per minute of footage. `cache move` moves what's cached to the new folder, say on a disk meant for media, and remembers it. Setting `ROSMO360_CACHE=/some/folder` overrides that for one run or a script.
- **Ahead of time**: run `cache add` after copying a card, and even the first open is quick. It skips clips already cached, so it's cheap to run again.
- **Safe to clear**: anything removed is simply read again the next time it's needed. `--no-cache` on any command reads clips afresh and caches nothing.
