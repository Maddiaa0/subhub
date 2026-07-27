fn main() {
    if let Err(error) = sub_manager::run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
