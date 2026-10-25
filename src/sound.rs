//! Sound for the live pane (Pointman's docs/dev/video.md, "Sound"): a camera's own microphone (a
//! webcam's, a capture card's HDMI audio, the Mac's beside its camera), or one the `audio` setting
//! names, or for the test card a sine with a beep each second. ffmpeg encodes it with its `aac` to
//! ADTS, 48 kHz mono.

use std::process::{Command, Stdio};

use anyhow::{bail, Result};
use pointman_extension::protocol::pane_open_response::video;
use regex::Regex;

use crate::devices::{Backend, Device};
use crate::ffmpeg;

/// What the live pane's sound is: always this, so the open can say it before any is captured.
pub const RATE: u32 = 48000;
pub const CHANNELS: u32 = 1;
/// Where ALSA lists its cards, if not /proc/asound/cards: for tests.
pub const ASOUND_ENV: &str = "CAM_ASOUND";

/// A microphone ffmpeg can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mic {
    /// avfoundation's audio index, or the ALSA card's number.
    pub id: String,
    pub name: String,
    /// What ffmpeg opens: avfoundation's name (or index), or ALSA's `plughw:CARD=<id>,DEV=0`.
    pub input: String,
}

/// Where a live pane's sound comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Mic(Mic),
    /// The test card's: a 440 Hz sine with a beep each second.
    Sine,
}

/// avfoundation's audio devices, from the same list as its cameras.
pub fn avfoundation_mics(log: &str) -> Vec<Mic> {
    let line = Regex::new(r"\] \[(\d+)\] (.+)$").expect("a regex");
    let mut mics = Vec::new();
    let mut audio = false;
    for l in log.lines() {
        if l.contains("AVFoundation audio devices") {
            audio = true;
        } else if l.contains("AVFoundation video devices") {
            audio = false;
        } else if let Some(c) = line.captures(l.trim_end()).filter(|_| audio) {
            let name = c[2].trim().to_string();
            // by name, which stays put as others come and go, unless it has avfoundation's colon
            let input = if name.contains(':') { c[1].to_string() } else { name.clone() };
            mics.push(Mic { id: c[1].to_string(), name, input });
        }
    }
    mics
}

/// ALSA's cards (`/proc/asound/cards`: ` 1 [C922           ]: USB-Audio - C922 Pro Stream Webcam`), each
/// opened through plughw by its id, which stays put when cards are renumbered.
pub fn alsa_mics(cards: &str) -> Vec<Mic> {
    let line = Regex::new(r"^\s*(\d+)\s+\[([^\]]+?)\s*\]:\s*.+?\s-\s(.+)$").expect("a regex");
    cards
        .lines()
        .filter_map(|l| line.captures(l.trim_end()))
        .map(|c| Mic {
            id: c[1].to_string(),
            name: c[3].trim().to_string(),
            input: format!("plughw:CARD={},DEV=0", &c[2]),
        })
        .collect()
}

/// This machine's microphones.
pub fn mics(avfoundation_list: Option<&str>) -> Vec<Mic> {
    match Backend::here() {
        Some(Backend::Avfoundation) => avfoundation_list.map(avfoundation_mics).unwrap_or_default(),
        Some(Backend::V4l2) => {
            let path = std::env::var_os(ASOUND_ENV).unwrap_or_else(|| "/proc/asound/cards".into());
            alsa_mics(&std::fs::read_to_string(path).unwrap_or_default())
        }
        None => Vec::new(),
    }
}

/// A name's words that say which device it is: lower case, without the ones every camera or
/// microphone has.
fn key(name: &str) -> Vec<String> {
    const COMMON: [&str; 9] = ["camera", "microphone", "mic", "webcam", "audio", "video", "input", "usb", "device"];
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !COMMON.contains(w))
        .map(String::from)
        .collect()
}

/// A camera's own microphone: the one whose name is the camera's but for those words (MacBook Pro
/// Camera's MacBook Pro Microphone, a Cam Link 4K's Cam Link 4K), or the start of it (a Desk View's).
pub fn own_mic<'a>(camera: &Device, mics: &'a [Mic]) -> Option<&'a Mic> {
    let cam = key(&camera.name);
    if cam.is_empty() {
        return None;
    }
    mics.iter().find(|m| {
        let mic = key(&m.name);
        !mic.is_empty() && (cam.starts_with(&mic) || mic.starts_with(&cam))
    })
}

/// The live pane's sound for `camera` under the `audio` setting: `camera` (its own microphone, or for
/// the test card its sine; a gphoto2 body's live view has none), `none`, or a microphone's number or
/// any part of its name.
pub fn pick(camera: &Device, setting: &str, mics: &[Mic], machine: &str) -> Result<Option<Source>> {
    let setting = setting.trim();
    match setting.to_lowercase().as_str() {
        "none" | "off" => Ok(None),
        "" | "camera" if camera.is_test() => Ok(Some(Source::Sine)),
        "" | "camera" => Ok(own_mic(camera, mics).cloned().map(Source::Mic)),
        asked => {
            let found = mics
                .iter()
                .find(|m| m.id == asked || m.name.to_lowercase() == asked)
                .or_else(|| mics.iter().find(|m| m.name.to_lowercase().contains(asked)));
            match found {
                Some(mic) => Ok(Some(Source::Mic(mic.clone()))),
                None => {
                    let have: Vec<String> = mics.iter().map(|m| format!("{} {}", m.id, m.name)).collect();
                    let have = if have.is_empty() { "none".to_string() } else { have.join(", ") };
                    bail!("no microphone '{setting}' on {machine} (the audio setting): it has {have}")
                }
            }
        }
    }
}

/// What the pane opens with for this sound.
pub fn opening() -> video::Audio {
    video::Audio {
        codec: video::audio::Codec::CODEC_AAC.into(),
        rate: RATE as i32,
        channels: CHANNELS as i32,
        ..Default::default()
    }
}

/// ffmpeg reading `source` as ADTS AAC on stdout, each packet as it's made.
pub fn command(source: &Source) -> Command {
    let mut cmd = ffmpeg::command();
    cmd.args(["-v", "error"]);
    match source {
        Source::Sine => {
            cmd.args(["-re", "-f", "lavfi", "-i", &format!("sine=frequency=440:beep_factor=4:sample_rate={RATE}")]);
        }
        Source::Mic(mic) => match Backend::here() {
            Some(Backend::V4l2) => {
                cmd.args(["-f", "alsa", "-i", &mic.input]);
            }
            _ => {
                cmd.args(["-f", "avfoundation", "-i", &format!(":{}", mic.input)]);
            }
        },
    }
    cmd.args(["-vn", "-c:a", "aac", "-b:a", "96k", "-ar", &RATE.to_string(), "-ac", &CHANNELS.to_string()]);
    cmd.args(["-flush_packets", "1", "-f", "adts", "pipe:1"]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::avfoundation_devices;

    /// mac-m4's list on 16 October.
    const MAC_M4: &str = "\
[AVFoundation indev @ 0x1] AVFoundation video devices:
[AVFoundation indev @ 0x1] [0] Sony Camera (Imaging Edge)
[AVFoundation indev @ 0x1] [1] MacBook Pro Camera
[AVFoundation indev @ 0x1] [2] OBS Virtual Camera
[AVFoundation indev @ 0x1] [3] iPhone (2) Camera
[AVFoundation indev @ 0x1] [4] MacBook Pro Desk View Camera
[AVFoundation indev @ 0x1] AVFoundation audio devices:
[AVFoundation indev @ 0x1] [0] BlackHole 2ch
[AVFoundation indev @ 0x1] [1] MacBook Pro Microphone
[AVFoundation indev @ 0x1] [2] iPhone (2) Microphone
[AVFoundation indev @ 0x1] [3] ZoomAudioDevice
[AVFoundation indev @ 0x1] [4] Cam Link 4K
";

    #[test]
    fn each_camera_has_its_own_microphone_or_none() {
        let (cams, mics) = (avfoundation_devices(MAC_M4), avfoundation_mics(MAC_M4));
        assert_eq!(mics.len(), 5);
        assert_eq!(
            mics[1],
            Mic { id: "1".into(), name: "MacBook Pro Microphone".into(), input: "MacBook Pro Microphone".into() }
        );
        let own: Vec<Option<&str>> = cams.iter().map(|c| own_mic(c, &mics).map(|m| m.name.as_str())).collect();
        assert_eq!(
            own,
            [None, Some("MacBook Pro Microphone"), None, Some("iPhone (2) Microphone"), Some("MacBook Pro Microphone")]
        );
        let card = Device::ffmpeg("5", "Cam Link 4K");
        assert_eq!(own_mic(&card, &mics).unwrap().name, "Cam Link 4K");
        assert_eq!(own_mic(&Device::ffmpeg("6", "USB Camera"), &mics), None, "nothing to tell it by");
    }

    #[test]
    fn the_audio_setting() {
        let (cams, mics) = (avfoundation_devices(MAC_M4), avfoundation_mics(MAC_M4));
        let pick = |cam: &Device, setting: &str| pick(cam, setting, &mics, "mac-m4");
        let mic = |name: &str| Some(Source::Mic(mics.iter().find(|m| m.name == name).unwrap().clone()));
        assert_eq!(pick(&cams[1], "camera").unwrap(), mic("MacBook Pro Microphone"));
        assert_eq!(pick(&cams[1], "").unwrap(), mic("MacBook Pro Microphone"));
        assert_eq!(pick(&cams[0], "camera").unwrap(), None); // Imaging Edge's has none
        assert_eq!(pick(&cams[1], "none").unwrap(), None);
        assert_eq!(pick(&cams[0], "iphone").unwrap(), mic("iPhone (2) Microphone"));
        assert_eq!(pick(&cams[0], "0").unwrap(), mic("BlackHole 2ch"));
        assert_eq!(pick(&Device::test(), "camera").unwrap(), Some(Source::Sine));
        let sony = Device {
            via: crate::devices::Via::Gphoto { port: "usb:1,2".into() },
            ..Device::ffmpeg("g0", "Sony Alpha-A6400 (PC Control)")
        };
        assert_eq!(pick(&sony, "camera").unwrap(), None, "live view has no sound");
        assert_eq!(pick(&sony, "macbook").unwrap(), mic("MacBook Pro Microphone"));
        let e = pick(&cams[1], "rode").unwrap_err().to_string();
        assert!(
            e.starts_with("no microphone 'rode' on mac-m4 (the audio setting): it has 0 BlackHole 2ch, 1 MacBook"),
            "{e}"
        );
    }

    #[test]
    fn alsas_cards() {
        let cards = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0xf7f10000 irq 32\n 1 [C922           ]: USB-Audio - C922 Pro Stream Webcam\n                      Logitech C922 at usb-0000:00:14.0-2\n";
        let mics = alsa_mics(cards);
        assert_eq!(mics.len(), 2);
        assert_eq!(
            mics[1],
            Mic { id: "1".into(), name: "C922 Pro Stream Webcam".into(), input: "plughw:CARD=C922,DEV=0".into() }
        );
        assert_eq!(own_mic(&Device::ffmpeg("0", "C922 Pro Stream Webcam"), &mics).unwrap().id, "1");
    }
}
