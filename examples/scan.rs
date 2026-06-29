//! Minimal use of sdirstat as a library:  cargo run --example scan -- /some/path
use sdirstat::{scan, Config};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let tree = scan(&path, &Config::default());
    eprintln!("{}: {} bytes across {} entries", tree.root(), tree.total(), tree.entries());
    let json = tree.to_json();
    println!("{}", &json[..json.len().min(160)]);
}
