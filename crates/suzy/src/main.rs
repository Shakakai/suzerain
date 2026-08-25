fn main() -> eframe::Result<()> {
    // Handle --version/-V before touching eframe/egui at all — suzy has no
    // other CLI surface (it's a GUI-only app), so this is a plain arg
    // check rather than pulling in a full parser for one flag.
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!("suzy {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    suzy::run()
}
