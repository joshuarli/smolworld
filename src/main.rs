fn main() {
    if let Err(error) = smolworld::run() {
        eprintln!("smolworld: {error}");
        std::process::exit(1);
    }
}
