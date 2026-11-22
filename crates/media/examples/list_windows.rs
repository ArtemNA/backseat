//! Prints the windows the source picker would offer to stream (title, program), one per line,
//! for tests: our own video windows must not be among them.

fn main() {
    for w in media::platform::windows() {
        println!("{}\t{}", w.title, w.exe);
    }
}
