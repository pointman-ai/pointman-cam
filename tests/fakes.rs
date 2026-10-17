//! cam under a stand-in node, with fake ffmpeg and gphoto2: finding cameras, stills, clips, and the live
//! pane starting a camera only while someone's watching.

mod common;

use std::thread::sleep;
use std::time::Duration;

use common::{fake_camera, fake_virtual, Cam, Tools};
use pointman_extension::testing::RunOptions;
use serde_json::json;

const WAIT: Duration = Duration::from_secs(10);

fn fakes() -> Tools {
    Tools { ffmpeg: Some(true), gphoto2: true, env: &[] }
}

#[test]
fn it_lists_ffmpegs_cameras_and_gphoto2s_and_the_test_card() {
    let cam = Cam::start(fakes(), json!({}));
    let done = cam.node.run("cam.list", json!({})).unwrap();
    assert_eq!(
        done.result["devices"],
        json!([{"id": "0", "name": fake_virtual(), "virtual": true}, {"id": "1", "name": fake_camera(), "virtual": false},
               {"id": "g0", "name": "Sony Alpha-A6400 (PC Control)", "virtual": false},
               {"id": "test", "name": "Test card", "virtual": false}])
    );
    // with none set, a body in PC Remote mode is the likeliest camera
    assert_eq!(done.result["default"], "g0");
    let text = done.result["text"].as_str().unwrap();
    assert!(text.starts_with("Cameras on mac-m4:\n"), "{text}");
    assert!(text.contains(&format!("- **0** {} (virtual)\n", fake_virtual())), "{text}");
    assert!(text.contains("- **g0** Sony Alpha-A6400 (PC Control) (the default)\n"), "{text}");
    // the screen and the iPhone, which can't take pictures, aren't cameras
    assert!(!text.contains("Capture screen") && !text.contains("iPhone"), "{text}");
}

#[test]
fn without_gphoto2_it_says_how_to_get_it() {
    let cam = Cam::start(Tools { gphoto2: false, ..fakes() }, json!({}));
    let done = cam.node.run("cam.list", json!({})).unwrap();
    assert_eq!(done.result["default"], "1", "the camera, not the virtual one listed first");
    let note = done.result["notes"][0].as_str().unwrap();
    assert!(
        note.starts_with("For cameras in PC Remote mode (Sony, Canon, Nikon, Fuji): gphoto2 isn't installed"),
        "{note}"
    );
    assert!(note.contains("brew install gphoto2") && note.contains("nix"), "{note}");
    assert!(done.result["text"].as_str().unwrap().contains("brew install gphoto2"));
    // a camera that isn't there says what is, and the note
    let e = cam.node.run("cam.still", json!({"device": "sony"})).unwrap_err().message;
    assert!(
        e.starts_with(&format!("no camera 'sony' on mac-m4: it has 0 {}, 1 {}", fake_virtual(), fake_camera())),
        "{e}"
    );
    assert!(e.contains("brew install gphoto2"), "{e}");
}

#[test]
fn a_still_waits_for_the_camera_to_settle_in_a_mode_it_has() {
    let cam = Cam::start(fakes(), json!({"quality": 3}));
    let done = cam.node.run("cam.still", json!({"device": "fake", "thread_id": 11576})).unwrap();
    assert_eq!(done.result["name"], fake_camera());
    assert_eq!((done.result["width"].clone(), done.result["height"].clone()), (json!(640), json!(424)));
    assert_eq!(done.outputs.len(), 1);
    let out = &done.outputs[0];
    assert_eq!((out.kind.as_str(), out.role.as_str()), ("image", "main"));
    assert_eq!(out.path.file_name().unwrap(), format!("{}.jpg", fake_camera()).as_str());
    assert_eq!(out.meta["camera"], fake_camera());
    let still = cam.calls().into_iter().find(|c| c.contains("-frames:v 1")).unwrap();
    if cfg!(target_os = "macos") {
        assert!(still.contains("-f avfoundation -video_size 1280x720 -framerate 30 -i Fake Camera"), "{still}");
    } else {
        assert!(
            still.contains("-f v4l2 -input_format mjpeg -video_size 1280x720 -framerate 15 -i /dev/video1"),
            "{still}"
        );
    }
    assert!(still.contains("select=gte(n\\,"), "a second of frames first: {still}");
    assert!(still.contains("-q:v 3"), "{still}");
}

#[test]
fn a_clip_says_how_far_it_is_and_is_published_as_a_video() {
    let cam = Cam::start(fakes(), json!({}));
    let done = cam.node.run("cam.clip", json!({"device": "fake", "seconds": 3})).unwrap();
    assert_eq!(done.result["seconds"], 3);
    assert_eq!((done.result["width"].clone(), done.result["height"].clone()), (json!(1280), json!(720)));
    assert_eq!(done.result["text"], format!("3 s from {}, 1280×720 at 15 fps", fake_camera()));
    assert_eq!(done.outputs[0].kind, "video");
    assert_eq!(std::fs::read(&done.outputs[0].path).unwrap(), b"fake mp4");
    let fractions: Vec<f64> = done.progress.iter().filter_map(|p| p.fraction).collect();
    assert!(fractions.windows(2).all(|w| w[0] <= w[1]) && fractions.iter().any(|f| *f > 0.3), "{fractions:?}");
    let call = cam.calls().into_iter().find(|c| c.contains("-progress")).unwrap();
    assert!(call.contains("-t 3") && call.contains("fps=15") && call.contains("-c:v libx264"), "{call}");
    // out of range
    let e = cam.node.run("cam.clip", json!({"seconds": 61})).unwrap_err().message;
    assert_eq!(e, "seconds is 1 to 60, not 61");
}

#[test]
fn a_clip_can_be_cancelled() {
    let cam = Cam::start(Tools { env: &[("CAM_CLIP_STEPS", "300")], ..fakes() }, json!({}));
    let node = &cam.node;
    std::thread::scope(|s| {
        let run = s.spawn(|| {
            let options = RunOptions { cid: Some("long".into()), ..Default::default() };
            node.run_with("cam.clip", json!({"device": "test", "seconds": 60}), options)
        });
        cam.wait_calls("-progress", 1, WAIT);
        sleep(Duration::from_millis(100));
        assert_eq!(node.cancel("long").unwrap()["found"], true);
        let failed = run.join().unwrap().unwrap_err();
        assert!(failed.cancelled, "{failed:?}");
    });
}

#[test]
fn the_live_pane_starts_the_camera_only_while_someone_watches() {
    let cam = Cam::start(fakes(), json!({}));
    let view = cam.node.open_pane("live", json!({"device": "fake"})).unwrap();
    assert_eq!(view.format().as_deref(), Some("video"));
    assert_eq!(view.title().as_deref(), Some(fake_camera()));
    let audio = json!({"codec": "aac", "rate": 48000, "channels": 1}); // its own microphone's
    assert_eq!(view.video(), json!({"codec": "mjpeg", "width": 1280, "height": 720, "fps": 15, "audio": audio}));
    sleep(Duration::from_millis(300));
    assert_eq!(cam.count("pipe:1"), 0, "the camera's light stays off: {:?}", cam.calls());

    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 5).unwrap();
    let frame = view.video_frames().pop().unwrap();
    assert_eq!((frame.width, frame.height), (640, 424));
    assert_eq!(frame.jpeg, std::fs::read(common::fixture("frame.jpg")).unwrap());
    let live = cam.calls().into_iter().find(|c| c.contains("-f mjpeg pipe:1")).unwrap();
    assert!(
        live.contains("fps=15,scale=w=trunc(min(1280\\,iw)/2)*2:h=-2") && live.contains("-c:v mjpeg -q:v 5"),
        "{live}"
    );

    // a smaller view: ffmpeg again at its rate and width; then a second path, which changes nothing
    view.hint(5, 600).unwrap();
    cam.wait_calls("-f mjpeg pipe:1", 2, WAIT);
    cam.wait_calls("ffmpeg stopped", 1, WAIT);
    let again = cam.calls().into_iter().filter(|c| c.contains("-f mjpeg pipe:1")).nth(1).unwrap();
    assert!(again.contains("fps=5,scale=w=trunc(min(640\\,iw)/2)*2"), "{again}");
    view.viewers(2).unwrap();
    view.hint(5, 640).unwrap(); // the same width, rounded
    sleep(Duration::from_millis(300));
    assert_eq!(cam.count("-f mjpeg pipe:1"), 2);

    // paused, then nobody's watching: the camera stops each time
    view.hint(0, 640).unwrap();
    cam.wait_calls("ffmpeg stopped", 2, WAIT);
    view.hint(15, 1280).unwrap();
    cam.wait_calls("-f mjpeg pipe:1", 3, WAIT);
    view.viewers(0).unwrap();
    cam.wait_calls("ffmpeg stopped", 3, WAIT);
    sleep(Duration::from_millis(300));
    let stopped = view.frame_count();
    sleep(Duration::from_millis(300));
    assert_eq!(view.frame_count(), stopped);
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn closing_the_pane_stops_the_camera() {
    let cam = Cam::start(fakes(), json!({}));
    let view = cam.node.open_pane("live", json!({"device": "fake"})).unwrap();
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 2).unwrap();
    view.close("no_viewer").unwrap();
    cam.wait_calls("ffmpeg stopped", 1, WAIT);
    assert_eq!(cam.count("-f mjpeg pipe:1"), 1);
}

#[test]
fn the_nodes_shutdown_stops_a_camera_still_being_watched() {
    // a pane left open when the node shuts cam down: its camera stops before cam exits, rather than when
    // ffmpeg next writes into a pipe nobody reads (screenext found such leftovers, #extensions "Cam" [12361])
    let cam = Cam::start(fakes(), json!({}));
    let view = cam.node.open_pane("live", json!({"device": "fake"})).unwrap();
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 2).unwrap();
    cam.node.stop();
    assert!(!cam.node.running(), "cam exited");
    assert!(cam.calls().iter().any(|c| c.contains("ffmpeg stopped")), "{:?}", cam.calls());
}

#[test]
fn a_camera_that_stops_sending_is_tried_again() {
    // the fake camera goes away after 3 frames each time
    let cam = Cam::start(Tools { env: &[("CAM_DIE_AFTER", "3")], ..fakes() }, json!({}));
    let view = cam.node.open_pane("live", json!({"device": "fake"})).unwrap();
    view.viewers(1).unwrap();
    cam.wait_calls("-f mjpeg pipe:1", 2, WAIT);
    view.wait(WAIT, |v| v.frame_count() >= 4).unwrap();
    assert!(cam.node.log().contains("it stopped after 3 frames"), "{}", cam.node.log());
    // asked its modes afresh before trying again
    let probe = if cfg!(target_os = "macos") { "-video_size 1x1" } else { "-list_formats" };
    assert!(cam.count(probe) >= 2, "{:?}", cam.calls());
}

#[test]
fn a_sony_in_pc_remote_mode_takes_a_real_photo() {
    let cam = Cam::start(fakes(), json!({}));
    let done = cam.node.run("cam.still", json!({"device": "sony"})).unwrap();
    assert_eq!(done.result["device"], "g0");
    assert_eq!((done.result["width"].clone(), done.result["height"].clone()), (json!(1200), json!(800)));
    let out = &done.outputs[0];
    assert_eq!(out.path.file_name().unwrap(), "Sony Alpha-A6400 (PC Control).jpg");
    assert_eq!(std::fs::read(&out.path).unwrap(), std::fs::read(common::fixture("photo.jpg")).unwrap());
    let calls = cam.calls();
    let shot: Vec<&String> = calls.iter().filter(|c| c.contains("--capture-image-and-download")).collect();
    // PTPCamera had it the first time (on a Mac, it's stopped first, then again before the retry)
    assert_eq!(shot.len(), 2, "{calls:?}");
    assert!(shot[1].contains("--port usb:020,007 --camera Sony Alpha-A6400 (PC Control)"), "{}", shot[1]);
    let stopped = calls.iter().filter(|c| c.as_str() == "killall PTPCamera ptpcamerad").count();
    assert_eq!(stopped, if cfg!(target_os = "macos") { 2 } else { 0 }, "{calls:?}");
}

#[test]
fn a_sonys_live_view_streams_and_a_still_pauses_it() {
    let cam = Cam::start(fakes(), json!({}));
    let view = cam.node.open_pane("live", json!({"device": "g0"})).unwrap();
    assert_eq!(view.title().as_deref(), Some("Sony Alpha-A6400 (PC Control)"));
    assert_eq!(view.video()["width"], 1024); // its size isn't known before its first frame
    assert_eq!(cam.count("--capture-movie"), 0);
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 3).unwrap();
    let movie = cam.calls().into_iter().find(|c| c.contains("--capture-movie")).unwrap();
    assert!(
        movie.ends_with("--port usb:020,007 --camera Sony Alpha-A6400 (PC Control) --capture-movie --stdout"),
        "{movie}"
    );
    assert_eq!(view.video_frames()[0].width, 640);

    // a still needs the camera: live view stops, the photo's taken, live view starts again
    std::fs::write(cam.dir.path().join("claimed"), "").unwrap(); // no claim failure this time
    let done = cam.node.run("cam.still", json!({"device": "g0", "width": 640})).unwrap();
    assert_eq!(done.result["width"], 640);
    let calls = cam.calls();
    let at = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap();
    assert!(at("gphoto2 stopped") < at("--capture-image-and-download"), "{calls:?}");
    cam.wait_calls("--capture-movie", 2, WAIT);
    let before = view.frame_count();
    view.wait(WAIT, |v| v.frame_count() > before).unwrap();

    // the next one opens at the size it sent
    let next = cam.node.open_pane("live", json!({"device": "sony"})).unwrap();
    assert_eq!((next.video()["width"].clone(), next.video()["height"].clone()), (json!(640), json!(424)));
    view.close("closed").unwrap();
    next.close("closed").unwrap();
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn the_pane_takes_hints_only() {
    let cam = Cam::start(fakes(), json!({}));
    let view = cam.node.open_pane("live", json!({"device": "test"})).unwrap();
    let e = view.input(json!({"kind": "tap", "block": "x"})).unwrap_err().message;
    assert_eq!(e, "the camera pane takes hints only");
    let e = cam.node.open_pane("live", json!({"device": "canon"})).err().unwrap().message;
    assert!(e.starts_with("no camera 'canon' on mac-m4"), "{e}");
}

#[test]
fn the_live_pane_has_the_cameras_own_sound_while_the_speakers_on() {
    let cam = Cam::start(fakes(), json!({}));
    let view = cam.node.open_pane("live", json!({"device": "fake"})).unwrap();
    assert_eq!(view.video()["audio"], json!({"codec": "aac", "rate": 48000, "channels": 1}));
    sleep(Duration::from_millis(300));
    assert_eq!(cam.count("-f adts"), 0, "no microphone open with no one watching");

    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.sound_count() >= 4 && v.frame_count() >= 3).unwrap();
    let call = cam.calls().into_iter().find(|c| c.contains("-f adts pipe:1")).unwrap();
    if cfg!(target_os = "macos") {
        assert!(call.contains("-f avfoundation -i :Fake Microphone"), "{call}");
    } else {
        assert!(call.contains("-f alsa -i plughw:CARD=Webcam,DEV=0"), "{call}");
    }
    assert!(
        call.contains("-c:a aac") && call.contains("-ar 48000 -ac 1") && call.contains("-flush_packets 1"),
        "{call}"
    );
    // whole ADTS frames, in order, timed by their samples
    let sounds = view.sounds();
    assert!(sounds.iter().all(|s| s.frames >= 2), "{sounds:?}");
    assert!(sounds.windows(2).all(|w| w[1].captured_ms > w[0].captured_ms), "{sounds:?}");

    // the speaker off stops the microphone, not the pictures or their rate
    view.audio_hint(false).unwrap();
    cam.wait_calls("ffmpeg sound stopped", 1, WAIT);
    let frames = view.frame_count();
    view.wait(WAIT, |v| v.frame_count() > frames + 2).unwrap();
    assert_eq!(cam.count("-f mjpeg pipe:1"), 1, "the pictures weren't restarted");
    view.audio_hint(true).unwrap();
    cam.wait_calls("-f adts pipe:1", 2, WAIT);
    // nobody watching: both stop
    view.viewers(0).unwrap();
    cam.wait_calls("ffmpeg sound stopped", 2, WAIT);
    cam.wait_calls("ffmpeg stopped", 1, WAIT);
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn the_audio_setting_picks_the_sound() {
    // none: no sound at all
    let cam = Cam::start(fakes(), json!({"audio": "none"}));
    let view = cam.node.open_pane("live", json!({"device": "fake"})).unwrap();
    assert!(view.video()["audio"].is_null(), "{}", view.video());
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 2).unwrap();
    assert_eq!(cam.count("-f adts"), 0);
    drop(cam);

    // a Sony's live view has none, unless a microphone is named
    let cam = Cam::start(fakes(), json!({}));
    assert!(cam.node.open_pane("live", json!({"device": "g0"})).unwrap().video()["audio"].is_null());
    let done = cam.node.run("cam.list", json!({})).unwrap();
    assert_eq!(done.result["microphones"][1]["name"], "Desk Mic");
    assert!(done.result["text"].as_str().unwrap().contains("Microphones (the audio setting takes one):\n- **0**"));
    drop(cam);
    let cam = Cam::start(fakes(), json!({"audio": "desk"}));
    let view = cam.node.open_pane("live", json!({"device": "g0"})).unwrap();
    assert_eq!(view.video()["audio"]["codec"], "aac");
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.sound_count() >= 2).unwrap();
    let call = cam.calls().into_iter().find(|c| c.contains("-f adts pipe:1")).unwrap();
    assert!(call.contains(if cfg!(target_os = "macos") { ":Desk Mic" } else { "plughw:CARD=Desk,DEV=0" }), "{call}");
    drop(cam);

    // one that isn't there says which are
    let cam = Cam::start(fakes(), json!({"audio": "rode"}));
    let e = cam.node.open_pane("live", json!({"device": "fake"})).err().unwrap().message;
    assert!(e.starts_with("no microphone 'rode' on mac-m4 (the audio setting): it has 0 "), "{e}");
}
