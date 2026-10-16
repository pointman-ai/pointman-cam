//! A real camera, by hand only (`cargo test --test real_camera -- --ignored --nocapture`): the machine's
//! default camera through the real ffmpeg, for sizes only. Its still is deleted as soon as its size is
//! read, its live frames are only ever in memory, and nothing of either is printed but sizes. The
//! camera's light comes on for a few seconds.

mod common;

use std::time::Duration;

use common::{real_ffmpeg, Cam, Tools};
use serde_json::{json, Value};

#[test]
#[ignore = "a real camera: run by hand"]
fn the_default_camera_for_sizes_only() {
    assert!(real_ffmpeg().is_some(), "needs ffmpeg");
    let cam = Cam::start(Tools { ffmpeg: Some(false), gphoto2: false, env: &[] }, json!({}));
    let list = cam.node.run("cam.list", json!({})).unwrap();
    println!("cameras: {}", list.result["devices"]);
    assert_ne!(list.result["default"], "test", "no real camera here");

    let still = cam.node.run("cam.still", json!({})).unwrap();
    let path = &still.outputs[0].path;
    let bytes = std::fs::read(path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9]));
    println!(
        "still: {} {}x{}, {} bytes (deleted)",
        still.result["name"], still.result["width"], still.result["height"], still.result["bytes"]
    );

    let view = cam.node.open_pane("live", Value::Null).unwrap();
    println!("live opened: {} {}", view.title().unwrap_or_default(), view.video());
    view.viewers(1).unwrap();
    view.wait(Duration::from_secs(20), |v| v.frame_count() >= 10).unwrap();
    let sizes: Vec<(u16, u16, usize)> = view.video_frames().iter().map(|f| (f.width, f.height, f.jpeg.len())).collect();
    view.viewers(0).unwrap();
    view.close("closed").unwrap();
    println!("live: {} frames, the last {:?}", view.frame_count(), &sizes[sizes.len().saturating_sub(3)..]);
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}
