fn main() {
    if let Err(error) = smolworld::run(std::env::args_os().skip(1)) {
        eprintln!("smolworld: {error}");
        std::process::exit(1);
    }
}
