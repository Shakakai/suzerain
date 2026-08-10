//! OTEL wiring for the daemons themselves (agents get theirs via the
//! manifest observability block, fanned out as env — see provision.rs).
//!
//! Enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (standard OTEL envs
//! honored: endpoint, headers). Otherwise plain local tracing only.

use anyhow::Result;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

pub fn init(default_filter: &str, service_name: &'static str) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into());
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;
        let resource = opentelemetry_sdk::Resource::builder()
            .with_service_name(service_name)
            .build();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();
        let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, service_name);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .with(otel_layer)
            .init();
        tracing::info!(service = service_name, "OTEL export enabled");
    } else {
        tracing_subscriber::registry().with(filter).with(fmt).init();
    }
    Ok(())
}
