//! cam with the real ffmpeg, on the test card (testsrc2 with its running time drawn on), which works on
//! any machine: a still, a clip, and the live pane at the rate and width its hints ask for. No real
//! camera is ever opened: the default camera is the test card. Skipped where there's no ffmpeg.

mod common;

use std::time::{Duration, Instant};

use common::{real_ffmpeg, Cam, Tools};
use pointman_extension::testing::RunOptions;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(20);

fn card() -> Option<Cam> {
    if real_ffmpeg().is_none() {
        eprintln!("skipped: no ffmpeg on this machine");
        return None;
    }
    Some(Cam::start(Tools { ffmpeg: Some(false), gphoto2: false, env: &[] }, json!({"device": "test"})))
}

#[test]
fn a_still_of_the_test_card() {
    let Some(cam) = card() else { return };
    let done = cam.node.run("cam.still", json!({"width": 640})).unwrap();
    assert_eq!(done.result["device"], "test");
    assert_eq!((done.result["width"].clone(), done.result["height"].clone()), (json!(640), json!(360)));
    let jpeg = std::fs::read(&done.outputs[0].path).unwrap();
    assert!(jpeg.starts_with(&[0xff, 0xd8]) && jpeg.ends_with(&[0xff, 0xd9]));
    assert_eq!(done.result["bytes"], jpeg.len());
    // no width: the setting's
    assert_eq!(cam.node.run("cam.still", json!({})).unwrap().result["width"], 1280);
}

#[test]
fn a_clip_of_the_test_card() {
    let Some(cam) = card() else { return };
    let started = Instant::now();
    let done = cam.node.run("cam.clip", json!({"seconds": 4})).unwrap();
    // the card comes at its own rate, as a camera does
    assert!(started.elapsed() >= Duration::from_secs(4), "{:?}", started.elapsed());
    assert_eq!((done.result["width"].clone(), done.result["height"].clone()), (json!(1280), json!(720)));
    let mp4 = std::fs::read(&done.outputs[0].path).unwrap();
    assert_eq!(&mp4[4..8], b"ftyp", "an MP4");
    // how far it is, on the way
    assert!(done.progress.iter().any(|p| p.fraction.is_some_and(|f| f > 0.0 && f < 1.0)), "{:?}", done.progress);
}

#[test]
fn a_clip_of_the_test_card_can_be_cancelled() {
    let Some(cam) = card() else { return };
    let node = &cam.node;
    std::thread::scope(|s| {
        let run = s.spawn(|| {
            node.run_with(
                "cam.clip",
                json!({"seconds": 60}),
                RunOptions { cid: Some("c".into()), ..Default::default() },
            )
        });
        std::thread::sleep(Duration::from_millis(1500));
        node.cancel("c").unwrap();
        let failed = run.join().unwrap().unwrap_err();
        assert!(failed.cancelled, "{failed:?}");
    });
}

#[test]
fn the_live_test_card_follows_its_hints() {
    let Some(cam) = card() else { return };
    let view = cam.node.open_pane("live", Value::Null).unwrap();
    assert_eq!(view.title().as_deref(), Some("Test card"));
    let audio = json!({"codec": "aac", "rate": 48000, "channels": 1}); // its sine's
    assert_eq!(view.video(), json!({"codec": "mjpeg", "width": 1280, "height": 720, "fps": 15, "audio": audio}));
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 5).unwrap();
    let frame = view.video_frames().pop().unwrap();
    assert_eq!((frame.width, frame.height), (1280, 720));

    view.hint(5, 640).unwrap();
    view.wait(WAIT, |v| v.video_frames().last().is_some_and(|f| f.width == 640)).unwrap();
    assert_eq!(view.video_frames().last().unwrap().height, 360);
    let (from, start) = (view.frame_count(), Instant::now());
    std::thread::sleep(Duration::from_secs(2));
    let rate = (view.frame_count() - from) as f64 / start.elapsed().as_secs_f64();
    assert!((3.0..=7.0).contains(&rate), "{rate} frames a second");

    view.hint(0, 640).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let paused = view.frame_count();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(view.frame_count(), paused);
    view.close("closed").unwrap();
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn the_test_cards_sound_is_a_real_sine_in_adts() {
    let Some(cam) = card() else { return };
    let view = cam.node.open_pane("live", Value::Null).unwrap();
    assert_eq!(view.video()["audio"], json!({"codec": "aac", "rate": 48000, "channels": 1}));
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.sound_count() >= 25).unwrap();
    // ffmpeg's own ADTS, in runs of two frames (2048 samples, 42.7 ms), timed by their samples
    let sounds = view.sounds();
    assert!(sounds.iter().all(|s| s.frames == 2), "{:?}", sounds.iter().map(|s| s.frames).collect::<Vec<_>>());
    let steps: Vec<u64> = sounds.windows(2).map(|w| w[1].captured_ms - w[0].captured_ms).collect();
    assert!(steps.iter().all(|s| (42..=43).contains(s)), "{steps:?}");
    // a second of sound comes in about a second
    let (from, start) = (view.sound_count(), Instant::now());
    std::thread::sleep(Duration::from_secs(2));
    let per_second = (view.sound_count() - from) as f64 / start.elapsed().as_secs_f64();
    assert!((18.0..=29.0).contains(&per_second), "{per_second} sends a second");

    view.audio_hint(false).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let off = view.sound_count();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(view.sound_count(), off);
    view.close("closed").unwrap();
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}
