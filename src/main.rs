fn main() {
    if let Err(error) = subhub::run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
