//! Logging (JSON lines on stdout, like the Java logstash format) and OpenTelemetry tracing
//! (OTLP/HTTP, parent-based ratio sampling).

use crate::config::{Config, OtelConfig};
use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    trace::{Sampler, SdkTracerProvider},
};
use std::time::Duration;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Flushes and stops span export on drop.
pub struct TelemetryGuard(Option<SdkTracerProvider>);

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = p.shutdown();
        }
    }
}

pub fn build_tracer_provider(cfg: &OtelConfig) -> anyhow::Result<SdkTracerProvider> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(cfg.traces_url.clone())
        .with_timeout(Duration::from_secs(5))
        .build()?;
    let resource = Resource::builder()
        .with_service_name("message-service")
        .with_attribute(KeyValue::new("k8s.pod.name", cfg.pod_name.clone()))
        .build();
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            cfg.sampling_probability,
        ))))
        .with_resource(resource)
        .build())
}

pub fn init(cfg: &Config) -> anyhow::Result<TelemetryGuard> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,hyper=warn,h2=warn,tower=warn"));
    let provider = if cfg.otel.export_enabled {
        Some(build_tracer_provider(&cfg.otel)?)
    } else {
        None
    };
    let otel_layer = provider
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("message-service")));

    let registry = tracing_subscriber::registry().with(filter).with(otel_layer);
    if cfg.log_format.eq_ignore_ascii_case("text") {
        registry.with(tracing_subscriber::fmt::layer()).try_init()?;
    } else {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(false),
            )
            .try_init()?;
    }
    Ok(TelemetryGuard(provider))
}
