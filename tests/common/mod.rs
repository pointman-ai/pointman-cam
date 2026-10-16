//! What the tests share: cam under the SDK's stand-in node, with fakes of the tools it drives on a PATH
//! of their own (so no test reaches a real camera, or stops the real PTPCamera), and real ffmpeg only
//! where a test asks for it.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pointman_extension::testing::StandIn;
use serde_json::Value;
use tempfile::TempDir;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// ffmpeg as far as cam needs it: a camera called Fake Camera (avfoundation, with a screen beside it
/// that's left out) or Fake Webcam (v4l2, through CAM_SYSFS), its modes, and stills, clips and live
/// MJPEG made of the fixture frame. Each call is a line in $CAM_CALLS.
pub const FFMPEG: &str = r#"#!/bin/sh
echo "ffmpeg $*" >> "$CAM_CALLS"
for last; do :; done
case "$*" in
  *-list_devices*)
    printf '[AVFoundation indev @ 0x1] AVFoundation video devices:\n[AVFoundation indev @ 0x1] [0] Fake Camera\n[AVFoundation indev @ 0x1] [1] Capture screen 0\n[AVFoundation indev @ 0x1] AVFoundation audio devices:\n[AVFoundation indev @ 0x1] [0] Fake Microphone\n[in#0 @ 0x2] Error opening input: Input/output error\n' >&2
    exit 1 ;;
  *"-video_size 1x1"*)
    printf '[avfoundation @ 0x1] Selected video size (1x1) is not supported by the device.\n[avfoundation @ 0x1] Supported modes:\n[avfoundation @ 0x1]   640x480@[1.000000 30.000000]fps\n[avfoundation @ 0x1]   1280x720@[1.000000 30.000000]fps\n[avfoundation @ 0x1]   1920x1080@[1.000000 30.000000]fps\n' >&2
    exit 1 ;;
  *"-list_formats all"*)
    printf '[video4linux2,v4l2 @ 0x1] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720\n[video4linux2,v4l2 @ 0x1] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720 1920x1080\n' >&2
    exit 1 ;;
  *-encoders*)
    echo ' V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC'
    exit 0 ;;
  *-progress*)
    trap 'echo "ffmpeg stopped" >> "$CAM_CALLS"; exit 255' INT TERM
    printf "Output #0, mp4, to '%s':\n  Stream #0:0: Video: h264 (avc1 / 0x31637661), yuv420p(progressive), 1280x720 [SAR 1:1 DAR 16:9], q=2-31, 15 fps\n" "$last" >&2
    i=0
    while [ $i -lt "${CAM_CLIP_STEPS:-3}" ]; do echo "out_time_us=${i}000000"; echo progress=continue; sleep 0.2; i=$((i+1)); done
    printf 'fake mp4' > "$last"
    echo progress=end
    exit 0 ;;
  *"-f mjpeg pipe:1"*)
    trap 'echo "ffmpeg stopped" >> "$CAM_CALLS"; exit 255' INT TERM
    n=0
    while :; do
      cat "$CAM_FRAME"
      n=$((n+1))
      if [ -n "$CAM_DIE_AFTER" ] && [ $n -ge "$CAM_DIE_AFTER" ]; then echo "[avfoundation] the camera went away" >&2; exit 1; fi
      sleep 0.05
    done ;;
  *"-frames:v 1"*)
    cp "$CAM_FRAME" "$last"
    exit 0 ;;
esac
echo "fake ffmpeg: nothing for $*" >&2
exit 1
"#;

/// gphoto2 as far as cam needs it: a Sony in PC Remote mode and an iPhone that can't take pictures,
/// a photo (with a RAW beside it), and live view made of the fixture frame. $CAM_CLAIM_ONCE: the first
/// capture finds the camera claimed, as PTPCamera does.
pub const GPHOTO2: &str = r#"#!/bin/sh
echo "gphoto2 $*" >> "$CAM_CALLS"
for last; do :; done
case "$*" in
  *--auto-detect*)
    printf 'Model                          Port\n----------------------------------------------------------\nSony Alpha-A6400 (PC Control)  usb:020,007\nApple iPhone (PTP mode)        usb:020,011\n'
    exit 0 ;;
  *--abilities*)
    case "$*" in
      *iPhone*) printf 'Abilities for camera             : Apple iPhone (PTP mode)\nCapture choices                  : Capture not supported by the driver\n' ;;
      *) printf 'Abilities for camera             : Sony Alpha-A6400 (PC Control)\nCapture choices                  :\n                                 : Image\n                                 : Preview\nConfiguration support            : yes\n' ;;
    esac
    exit 0 ;;
  *--capture-image-and-download*)
    if [ -n "$CAM_CLAIM_ONCE" ] && [ ! -e "$CAM_CLAIM_ONCE" ]; then
      : > "$CAM_CLAIM_ONCE"
      echo '*** Error (-53: Could not claim the USB device) ***' >&2
      exit 1
    fi
    dir=$(dirname "$last")
    cp "$CAM_PHOTO" "$dir/photo.jpg"
    printf 'raw' > "$dir/photo.arw"
    echo "New file is in location /capt0000.jpg on the camera"
    echo "Saving file as $dir/photo.jpg"
    exit 0 ;;
  *--capture-movie*)
    trap 'echo "gphoto2 stopped" >> "$CAM_CALLS"; exit 0' INT TERM
    echo "Capturing preview frames as movie to 'stdout'. Press Ctrl-C to abort." >&2
    while :; do cat "$CAM_FRAME"; sleep 0.05; done ;;
esac
echo "*** Error: nothing for $* ***" >&2
exit 1
"#;

pub const KILLALL: &str = "#!/bin/sh\necho \"killall $*\" >> \"$CAM_CALLS\"\nexit 1\n";

pub fn fixture(name: &str) -> PathBuf {
    Path::new(ROOT).join("tests/fixtures").join(name)
}

/// The real ffmpeg, if this machine has one.
pub fn real_ffmpeg() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("ffmpeg")).find(|p| p.is_file())
}

/// Which tools a test's PATH has.
pub struct Tools {
    /// The fake ffmpeg, else the real one (or none, to see cam say so).
    pub ffmpeg: Option<bool>,
    pub gphoto2: bool,
    /// More for the fakes' environment (CAM_DIE_AFTER, CAM_CLIP_STEPS).
    pub env: &'static [(&'static str, &'static str)],
}

/// cam with these tools and settings, and where its fakes say what they were asked.
pub struct Cam {
    pub node: StandIn,
    pub dir: TempDir,
}

impl Cam {
    pub fn start(tools: Tools, settings: Value) -> Cam {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: &str| {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        script("killall", KILLALL);
        match tools.ffmpeg {
            Some(true) => script("ffmpeg", FFMPEG),
            Some(false) => {
                std::os::unix::fs::symlink(real_ffmpeg().expect("a real ffmpeg"), bin.join("ffmpeg")).unwrap()
            }
            None => {}
        }
        if tools.gphoto2 {
            script("gphoto2", GPHOTO2);
        }
        // one fake webcam for v4l2, as sysfs lists it
        let sysfs = dir.path().join("sysfs/video0");
        fs::create_dir_all(&sysfs).unwrap();
        fs::write(sysfs.join("name"), "Fake Webcam\n").unwrap();
        fs::write(sysfs.join("index"), "0\n").unwrap();
        let calls = dir.path().join("calls");
        fs::write(&calls, "").unwrap();
        let path = std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        let mut node = StandIn::new(ROOT);
        for (key, value) in tools.env {
            node = node.env(key, value);
        }
        let node = node
            .binary(env!("CARGO_BIN_EXE_cam"))
            .settings(settings)
            .machine("mac-m4")
            .env("PATH", path)
            .env("CAM_CALLS", &calls)
            .env("CAM_FRAME", fixture("frame.jpg"))
            .env("CAM_PHOTO", fixture("photo.jpg"))
            .env("CAM_SYSFS", dir.path().join("sysfs"))
            .env("CAM_CLAIM_ONCE", dir.path().join("claimed"))
            .start()
            .unwrap();
        Cam { node, dir }
    }

    /// What the fakes were asked, a line each.
    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("calls")).unwrap().lines().map(String::from).collect()
    }

    pub fn count(&self, needle: &str) -> usize {
        self.calls().iter().filter(|c| c.contains(needle)).count()
    }

    /// Wait until `needle` has been asked `n` times.
    pub fn wait_calls(&self, needle: &str, n: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.count(needle) < n {
            assert!(Instant::now() < deadline, "no {n} of '{needle}' in {:?}:\n{}", timeout, self.calls().join("\n"));
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The fake camera's name on this platform.
pub fn fake_camera() -> &'static str {
    if cfg!(target_os = "macos") {
        "Fake Camera"
    } else {
        "Fake Webcam"
    }
}
