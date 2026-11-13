//! Probe for capturing "all sound" one program at a time. Lists the programs with sound
//! sessions and what would be captured; with --capture N, captures each of those (its process
//! tree) for N seconds, mixes them, and prints once a second the level of each and of the mix.
//!
//!   audioprobe [--capture 10]

use gst::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() {
    media::init();
    let mut args = std::env::args().skip(1);
    let mut secs = 0u64;
    let mut only = None::<String>;
    let mut exclude = false;
    let mut pid_arg = None::<u32>;
    while let Some(a) = args.next() {
        if a == "--capture" {
            secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(10);
        } else if a == "--pid" {
            pid_arg = args.next().and_then(|v| v.parse().ok());
        } else if a == "--exclude" {
            exclude = true;
        } else if a == "--only" {
            only = args.next().map(|v| v.to_ascii_lowercase());
        }
    }
    let apps = media::audio_apps::audio_apps();
    for a in &apps {
        println!("app pid {} {} active {} router {}", a.pid, a.exe, a.active, media::audio_apps::is_router(&a.exe));
    }
    let take: Vec<_> = media::audio_apps::to_capture(&apps).into_iter().filter(|a| only.as_ref().is_none_or(|o| a.exe.to_ascii_lowercase().starts_with(o.as_str()))).collect();
    println!("to capture: {}", take.iter().map(|a| format!("{} ({})", a.exe, a.pid)).collect::<Vec<_>>().join(", "));
    if secs == 0 {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let mixer = gst::ElementFactory::make("audiomixer").property("latency", 20_000_000u64).build().unwrap();
    let caps = gst::Caps::builder("audio/x-raw").field("format", "F32LE").field("rate", 48_000i32).field("channels", 2i32).build();
    let mix_level = gst::ElementFactory::make("level").name("mix").property("interval", 1_000_000_000u64).build().unwrap();
    let sink = gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
    pipeline.add_many([&mixer, &mix_level, &sink]).unwrap();
    gst::Element::link_many([&mixer, &mix_level, &sink]).unwrap();
    let started = std::time::Instant::now();
    let targets: Vec<(u32, &str)> = if let Some(p) = pid_arg { vec![(p, "include-process-tree")] } else if exclude { vec![(std::process::id(), "exclude-process-tree")] } else { take.iter().map(|a| (a.pid, "include-process-tree")).collect() };
    for (pid, mode) in targets {
        let src = gst::ElementFactory::make("wasapi2src").property("loopback", true).property("low-latency", true).build().unwrap();
        // STREAMER_PROBE_MODE_FIRST: the mode before the pid (as host.rs does).
        if std::env::var_os("STREAMER_PROBE_MODE_FIRST").is_some() {
            src.set_property_from_str("loopback-mode", mode);
            src.set_property("loopback-target-pid", pid);
        } else {
            src.set_property("loopback-target-pid", pid);
            src.set_property_from_str("loopback-mode", mode);
        }
        let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
        let res = gst::ElementFactory::make("audioresample").build().unwrap();
        let cf = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap();
        let lv = gst::ElementFactory::make("level").name(format!("app{pid}")).property("interval", 1_000_000_000u64).build().unwrap();
        pipeline.add_many([&src, &conv, &res, &cf, &lv]).unwrap();
        gst::Element::link_many([&src, &conv, &res, &cf, &lv]).unwrap();
        lv.link(&mixer).unwrap();
    }
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let l2 = lines.clone();
    pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
        match msg.view() {
            gst::MessageView::Element(e) => {
                if let (Some(s), Some(src)) = (e.structure(), msg.src()) {
                    if s.name() == "level" {
                        if let Ok(rms) = s.get::<gst::glib::ValueArray>("rms") {
                            let db = rms.iter().filter_map(|v| v.get::<f64>().ok()).fold(-200f64, f64::max);
                            l2.lock().unwrap().push(format!("{} {:.0}", src.name(), db));
                        }
                    }
                }
            }
            gst::MessageView::Error(e) => eprintln!("error from {:?}: {}", e.src().map(|s| s.name()), e.error()),
            gst::MessageView::Latency(_) => println!("latency message"),
            _ => {}
        }
        gst::BusSyncReply::Drop
    });
    let r = pipeline.set_state(gst::State::Playing);
    println!("playing: {r:?} after {} ms", started.elapsed().as_millis());
    for _ in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        let mut l = lines.lock().unwrap();
        println!("levels: {}", l.join(", "));
        l.clear();
    }
    let mut q = gst::query::Latency::new();
    if pipeline.query(&mut q) {
        let (live, min, max) = q.result();
        println!("pipeline latency: live {live} min {min} max {max:?}");
    }
    let _ = pipeline.set_state(gst::State::Null);
}
