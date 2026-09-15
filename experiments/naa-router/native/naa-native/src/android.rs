fn main() {
    if let Err(error) = naa_native::server::run() {
        eprintln!("naa-native: {error}");
        std::process::exit(1)
    }
}
