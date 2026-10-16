//! cam: stills, clips and a live pane from any camera a machine can see (Pointman's docs/dev/video.md).
//! `bin/cam` is the extension its node runs ([`serve`]); the rest is how it finds and reads cameras.

pub mod adts;
pub mod devices;
pub mod ffmpeg;
pub mod gphoto;
pub mod jpeg;
pub mod live;
pub mod proc;
pub mod serve;
pub mod sound;
