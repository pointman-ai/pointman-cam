# cam

A Pointman extension: stills, clips and a live pane from any camera a node can see. The built-in camera,
USB webcams, a phone's camera (Continuity Camera), capture cards, and most Sony, Canon, Nikon and Fuji
bodies over USB, with no app from their maker. It's generic, tied to no other extension. Its live pane is a
Pointman video pane: the node seals each frame and sends it straight to the app that opened it, never
through Pointman's server.

One Rust binary, `bin/cam`, built for each platform and written with the
[pointman-extension](https://github.com/pointman-ai/pointman-extension) crate.

## Cameras

- **ffmpeg** reads what the system sees as a camera: avfoundation's video devices on macOS (not its
  screens), v4l2's capture nodes on Linux. A camera in USB streaming mode is one of these.
- **gphoto2** reads a camera in its PC Remote mode on USB, the protocol its maker's own app uses: live view
  as MJPEG, and a still that's a real photo, taken by the camera at its full size. On macOS, PTPCamera
  grabs every such camera, so cam stops it just before gphoto2 needs the camera (launchd starts it
  again later).
- **test** is a test card (ffmpeg's testsrc2, with its running time drawn on) that works anywhere.

A camera is named by its number (`0`, `g0` for gphoto2's), its name, or any part of its name, in any case:
`device=sony`. With none named, the `device` setting decides. With none set, cam takes the likeliest real
camera: a body in PC Remote mode, then the first camera that isn't a Desk View or virtual, and a virtual
one (OBS's, Imaging Edge Webcam's, Snap Camera, Camo…, by their names) only when there's nothing else.
`cam.list` marks the virtual ones. Nothing
needs setting up on a camera: cam asks each one which sizes and rates it has and picks one of those
(avfoundation takes only a rate at the top of one of a mode's ranges; v4l2 gets MJPEG where the camera has
it). A camera that's unplugged and plugged back in is found again by name.

**A Sony a6400 (and most bodies):** set its USB connection to PC Remote once, plug it in, and it's in
`cam.list`. Install gphoto2 first: `brew install gphoto2`
on a Mac, `apt install gphoto2` on Debian or Ubuntu, or nix's `gphoto2`.

## Commands

Run on a machine like any extension's: `/cam list`, `/cam still sony`, `/cam clip 0 20` in a thread, or
`run_on_machine` for agents.

| Kind | Params | What |
|---|---|---|
| `cam.list` | | `{devices: [{id, name, virtual}], microphones: [{id, name}], default, notes}`: what's missing to find more, too |
| `cam.still` | `device?`, `width?`, `thread_id?` | One JPEG, published to the thread like any file. A gphoto2 camera's is its full photo, unless a `width` is asked for |
| `cam.clip` | `device?`, `seconds` (1–60, default 10), `thread_id?` | An H.264 MP4, published; cancellable |

A still or clip on a camera only one program can use at a time (gphoto2's, and any on Linux) pauses a live
pane on it, and starts it again after.

## The live pane

`live` is a `video` pane (`opens = ["menu", "command"]`, target `{device}`): the camera's frames as JPEGs,
sealed by the node and sent straight to the app that opened it.

- The camera runs only while a path is attached (`pane.viewers` above 0), so its light is off otherwise.
- The app's hints set its rate and width (`max_fps: 0` pauses it), within the settings; a width is rounded
  up to 320, 480, 640, 960, 1280, 1920, 2560 or 3840, so resizing a window doesn't restart the camera at
  each pixel.
- ffmpeg (or gphoto2) writes MJPEG, cut into whole JPEGs by their markers. The newest waits in one slot
  and goes out as `pane.frame`, so a slow viewer never backs up the camera.
- If the camera stops sending, cam tries again 1 s later, doubling to 30 s, and finds it again by name.

**Sound,** when there's any: the camera's own microphone (a webcam's, a capture
card's HDMI audio, the Mac's beside its built-in camera: the microphone whose name is the camera's), or
the one the `audio` setting names; for the test card, a 440 Hz sine with a beep each second. A gphoto2
body's live view has none unless `audio` names a microphone. The open says so in its `video`: `{"codec":
"aac", "rate": 48000, "channels": 1}`.

- A second ffmpeg (avfoundation's audio device on macOS, ALSA through plughw on Linux) encodes it with its
  `aac` to ADTS. Whole frames go out in order as `pane.audio`, two at a time (43 ms), each timed by its
  samples rather than by when it arrived.
- It runs while a path is attached and the app's speaker is on: `audio: false` stops the microphone, and
  the pictures carry on. A hint says only what changed, so the speaker's leaves the rate and width alone.

## Settings

Per machine, in `~/.config/pointman/extensions/cam/config.toml`:

| Setting | Default | |
|---|---|---|
| `device` | the likeliest | The camera when none is named |
| `audio` | `camera` | The live pane's sound: `camera` (its own microphone), `none`, or a microphone's number or any part of its name |
| `fps` | 15 | Frames a second, for the live pane and clips |
| `width` | 1280 | The most pixels across, for stills, clips and the live pane |
| `quality` | 5 | ffmpeg's `-q:v` for stills and the live pane: lower is better and bigger |

## Development

`cargo test`. The tests run cam under the SDK's stand-in node with fake `ffmpeg`, `gphoto2` and `killall`
on a PATH of their own (tests/common), so none reaches a real camera or the real PTPCamera, and the test
card with the real ffmpeg (skipped without one). CI checks `cargo fmt` and `cargo clippy` too.

A release is a tag that's extension.toml's version: .github/workflows/release.yml builds the four
platforms and attaches them, as rota's does.
