//! Times the source-picker pictures for every window and monitor on this machine. Prints
//! only counts, times and sizes: no picture is saved or shown.

use std::time::Instant;

fn main() {
    let windows = media::platform::windows();
    let monitors = media::platform::monitors();
    for round in 1..=3 {
        let t = Instant::now();
        let (mut ok, mut empty, mut bytes) = (0, 0, 0usize);
        let mut slowest = 0u128;
        for w in &windows {
            let t1 = Instant::now();
            match media::thumbs::window(w.hwnd, 256, 144) {
                Some(s) => {
                    ok += 1;
                    bytes += s.len();
                }
                None => empty += 1,
            }
            if let Some(i) = media::thumbs::icon(w.hwnd, 20) {
                bytes += i.len();
            }
            slowest = slowest.max(t1.elapsed().as_millis());
        }
        let tw = t.elapsed().as_millis();
        let t = Instant::now();
        let shots = monitors.iter().filter_map(|m| media::thumbs::monitor(m.handle, 256, 144)).count();
        println!(
            "round {round}: {} windows in {tw} ms ({ok} pictures, {empty} empty, slowest {slowest} ms), {shots}/{} monitors in {} ms, {} KB total",
            windows.len(),
            monitors.len(),
            t.elapsed().as_millis(),
            bytes / 1024
        );
    }
}
