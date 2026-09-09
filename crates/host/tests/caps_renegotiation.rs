//! Does changing the capture capsfilter mid-stream actually survive?
//!
//! Adaptive resolution (`media::quality`) retunes the `vcaps` capsfilter on a
//! live pipeline to drop the encode resolution on a hostile route. That is the
//! one genuinely risky part of the feature: if the renegotiation stalls the
//! graph, the symptom is a frozen stream - which is *exactly* the black screen
//! the feature exists to prevent, and would be indistinguishable from it in the
//! field.
//!
//! The production guard reverts and disables adaptation if frames stop after a
//! change, so the worst case is bounded. This test checks the mechanism itself,
//! on the real GPU encoder, without needing a browser: build the same element
//! chain the pipeline uses, count encoded buffers, change the caps, and require
//! that buffers keep coming out at the new size.
//!
//! Windows + a D3D11 encoder only, and skipped (not failed) when the elements
//! are unavailable, so it is a no-op on the dev box and meaningful on the host.

#![cfg(windows)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

/// Wait until `f` holds, or `dur` elapses. Returns whether it held.
fn wait_until(dur: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn make(name: &str) -> Option<gst::Element> {
    gst::ElementFactory::make(name).build().ok()
}

#[test]
fn changing_the_capsfilter_resolution_does_not_stall_the_pipeline() {
    gst::init().expect("gst init");

    // Mirror the production chain (media/pipeline.rs), with videotestsrc in
    // place of the desktop-duplication source so the test needs no capture
    // permissions or an interactive session.
    let Some(src) = make("videotestsrc") else {
        eprintln!("skip: no videotestsrc");
        return;
    };
    src.set_property("is-live", true);
    let (Some(upload), Some(convert)) = (make("d3d11upload"), make("d3d11convert")) else {
        eprintln!("skip: no d3d11upload/d3d11convert - not a D3D11 machine");
        return;
    };
    // Whichever hardware encoder this box actually has.
    let Some(enc) = ["nvd3d11h264enc", "qsvh264enc", "amfh264enc", "openh264enc"]
        .iter()
        .find_map(|n| make(n))
    else {
        eprintln!("skip: no usable H.264 encoder");
        return;
    };
    let Some(sink) = make("fakesink") else {
        eprintln!("skip: no fakesink");
        return;
    };
    sink.set_property("sync", false);

    let caps_at = |w: i32, h: i32| {
        gst::Caps::builder("video/x-raw")
            .features(["memory:D3D11Memory"])
            .field("format", "NV12")
            .field("width", w)
            .field("height", h)
            .field("framerate", gst::Fraction::new(60, 1))
            .build()
    };
    let vcaps = gst::ElementFactory::make("capsfilter")
        .name("vcaps")
        .property("caps", caps_at(2560, 1440))
        .build()
        .expect("capsfilter");

    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&src, &upload, &convert, &vcaps, &enc, &sink])
        .expect("add");
    if gst::Element::link_many([&src, &upload, &convert, &vcaps, &enc, &sink]).is_err() {
        eprintln!("skip: elements would not link on this machine");
        return;
    }

    // Count encoded buffers and record the width the encoder is emitting.
    let buffers = Arc::new(AtomicU64::new(0));
    let width = Arc::new(AtomicU32::new(0));
    {
        let buffers = buffers.clone();
        let width = width.clone();
        let pad = enc.static_pad("src").expect("encoder src pad");
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
            buffers.fetch_add(1, Ordering::Relaxed);
            if let Some(w) = pad
                .current_caps()
                .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("width").ok()))
            {
                width.store(w as u32, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }

    if pipeline.set_state(gst::State::Playing).is_err() {
        eprintln!("skip: pipeline would not start on this machine");
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    let flowing = wait_until(Duration::from_secs(10), || {
        buffers.load(Ordering::Relaxed) > 10
    });
    assert!(flowing, "pipeline never produced encoded buffers at 1440p");
    assert_eq!(
        width.load(Ordering::Relaxed),
        2560,
        "expected to start at 1440p"
    );

    // The operation under test: retune resolution on the running graph.
    let before = buffers.load(Ordering::Relaxed);
    vcaps.set_property("caps", caps_at(1280, 720));

    let still_flowing = wait_until(Duration::from_secs(10), || {
        buffers.load(Ordering::Relaxed) > before + 20
    });
    let after = buffers.load(Ordering::Relaxed);
    let final_w = width.load(Ordering::Relaxed);
    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        still_flowing,
        "the graph stalled after the caps change: {before} buffers before, {after} after. \
         Adaptive resolution would freeze the stream on this encoder."
    );
    assert_eq!(
        final_w, 1280,
        "the encoder kept emitting {final_w}px wide - the caps change did not take effect"
    );
}
