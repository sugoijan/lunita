//! Runs chunk inputs (files or directories of them) one after the other, as the target does, and
//! prints the heap left in use every 1,000 inputs, to tell a leak from the fuzzer's own growth:
//! `cargo run --release --example heap -- <corpus dir>...`.

fn main() {
    let mut paths = Vec::new();
    for arg in std::env::args().skip(1) {
        match std::fs::read_dir(&arg) {
            Ok(dir) => paths.extend(dir.map(|entry| entry.unwrap().path())),
            Err(_) => paths.push(arg.into()),
        }
    }
    paths.sort();
    let start = lunita_fuzz::heap_in_use();
    println!("{} inputs, heap at start {start}", paths.len());
    for (i, path) in paths.iter().enumerate() {
        let data = std::fs::read(path).unwrap();
        let (compat, source) = lunita_fuzz::split_compat(&data);
        lunita_fuzz::run(source, compat);
        if (i + 1) % 1000 == 0 || i + 1 == paths.len() {
            println!("{}: heap {}", i + 1, lunita_fuzz::heap_in_use());
        }
    }
}
