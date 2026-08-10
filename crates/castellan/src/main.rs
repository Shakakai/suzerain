fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "castellan=info".into()),
        )
        .init();
    tracing::info!("castellan daemon (Phase 0 scaffold — see spikes and docs/PLAN.md)");
}
