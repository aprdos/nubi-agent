use std::time::Duration;

use tracing_subscriber::prelude::*;

pub fn init_telemetry(service_name: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer().json().with_target(true);

    let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
            .expect("failed to create OTLP span exporter");

        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new("service.name", service_name.to_string()),
            ]))
            .build();

        let tracer =
            opentelemetry::trace::TracerProvider::tracer(&provider, service_name.to_string());
        let otel_layer = tracing_opentelemetry::OpenTelemetryLayer::new(tracer);

        registry.with(otel_layer).init();
    } else {
        registry.init();
    }
}

pub fn record_turn(harness: &str, result: &str, duration: Duration) {
    let labels = [
        ("harness", harness.to_string()),
        ("result", result.to_string()),
    ];
    metrics::counter!("nubi_agent_turns_total", &labels).increment(1);
    metrics::histogram!(
        "nubi_agent_turn_duration_seconds",
        &[("harness", harness.to_string())]
    )
    .record(duration.as_secs_f64());
}

pub fn record_nubi_request(method: &str, status: &str) {
    let labels = [
        ("method", method.to_string()),
        ("status", status.to_string()),
    ];
    metrics::counter!("nubi_agent_nubi_requests_total", &labels).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_turn_does_not_panic() {
        record_turn("claude", "posted", Duration::from_millis(500));
        record_turn("codex", "failed", Duration::from_millis(100));
    }

    #[test]
    fn record_nubi_request_does_not_panic() {
        record_nubi_request("POST", "200");
        record_nubi_request("GET", "500");
    }
}
