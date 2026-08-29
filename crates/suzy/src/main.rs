fn main() -> eframe::Result<()> {
    // Handle a few flags before touching eframe/egui at all — suzy has no
    // other CLI surface (it's a GUI-only app), so this is a plain arg check
    // rather than pulling in a full parser. `--print-operator-id` and
    // `--add-workspace` exist for scripted setup (see ops/dev-suzy.sh /
    // `mise run dev:suzy`, and docs/SUZY.md §6.4): they let a wiring script
    // read Suzy's own iroh identity and pre-populate its workspace config
    // without ever opening the GUI, so by the time it *does* open, a
    // workspace is already connected.
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("suzy {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // `--debug <view>` boots straight into one named view filled with
    // fixture data, no live control plane needed — for a human or an
    // agent to screenshot a view and check its styling against
    // crates/suzy/design-system/ (see src/debug.rs).
    if let Some(pos) = args.iter().position(|a| a == "--debug") {
        let Some(view) = args.get(pos + 1) else {
            eprintln!(
                "usage: suzy --debug <view>  (one of: {})",
                suzy::debug::VIEW_NAMES.join(", ")
            );
            std::process::exit(1);
        };
        return suzy::run_debug(view);
    }

    if args.iter().any(|a| a == "--print-operator-id") {
        match suzy::config::load_or_create_key() {
            Ok(key) => println!("{}", key.public()),
            Err(e) => {
                eprintln!("suzy: failed to load/create operator key: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(pos) = args.iter().position(|a| a == "--add-workspace") {
        let (Some(name), Some(endpoint_id)) = (args.get(pos + 1), args.get(pos + 2)) else {
            eprintln!("usage: suzy --add-workspace <name> <endpoint_id>");
            std::process::exit(1);
        };
        let mut cfg = suzy::config::load();
        match cfg.workspaces.iter_mut().find(|w| &w.name == name) {
            Some(existing) => existing.endpoint_id = endpoint_id.clone(),
            None => cfg.workspaces.push(suzy::config::WorkspaceCfg {
                name: name.clone(),
                endpoint_id: endpoint_id.clone(),
                test_addr: None,
            }),
        }
        if let Err(e) = suzy::config::save(&cfg) {
            eprintln!("suzy: failed to save config: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    suzy::run()
}
