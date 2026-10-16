//! The cam extension, as its node runs it.

fn main() {
    if let Err(e) = cam::serve::serve() {
        eprintln!("cam: {e:#}");
        std::process::exit(1);
    }
}
