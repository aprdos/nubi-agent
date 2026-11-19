use std::time::Duration;

use tracing_subscriber::prelude::*;

pub fn init_telemetry(service_name: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer().json().with_target(true);

    let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = match opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
        {
            Ok(e) => e,
            Err(e) => {
                // Subscriber not installed yet — eprintln is the only output path.
                eprintln!(
                    "[nubi-agent] OTel span exporter failed to initialize, tracing disabled: {e}"
                );
                registry.init();
                return;
            }
        };

        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new("service.name", service_name.to_string()),
            ]))
            .build();

        let tracer =
            opentelemetry::trace::TracerProvider::tracer(&provider, service_name.to_string());
        opentelemetry::global::set_tracer_provider(provider);
        let otel_layer = tracing_opentelemetry::OpenTelemetryLayer::new(tracer);

        registry.with(otel_layer).init();
    } else {
        registry.init();
    }
}

pub fn shutdown_telemetry() {
    opentelemetry::global::shutdown_tracer_provider();
}

pub fn record_turn(harness: &str, result: &str, duration: Duration) {
    tracing::info!(
        harness,
        result,
        elapsed_ms = duration.as_millis() as u64,
        "turn.complete"
    );
}

pub fn record_nubi_request(method: &str, status: &str, endpoint: &str) {
    tracing::info!(method, status, endpoint, "nubi.request");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct CapturedEvent {
        message: String,
        fields: Vec<(String, String)>,
    }

    struct CaptureLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(CapturedEvent {
                message: visitor.message,
                fields: visitor.fields,
            });
        }
    }

    #[derive(Default)]
    struct FieldVisitor {
        message: String,
        fields: Vec<(String, String)>,
    }

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{:?}", value);
            } else {
                self.fields
                    .push((field.name().to_string(), format!("{:?}", value)));
            }
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else {
                self.fields
                    .push((field.name().to_string(), value.to_string()));
            }
        }
    }

    fn capture_subscriber() -> (impl tracing::Subscriber, Arc<Mutex<Vec<CapturedEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            events: events.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        (subscriber, events)
    }

    fn find_field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn record_turn_emits_harness_result_and_elapsed() {
        let (sub, events) = capture_subscriber();
        with_default(sub, || {
            record_turn("claude", "posted", Duration::from_millis(500));
        });
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].message, "turn.complete");
        assert_eq!(find_field(&captured[0].fields, "harness"), Some("claude"));
        assert_eq!(find_field(&captured[0].fields, "result"), Some("posted"));
        assert_eq!(find_field(&captured[0].fields, "elapsed_ms"), Some("500"));
    }

    #[test]
    fn record_nubi_request_emits_method_and_status() {
        let (sub, events) = capture_subscriber();
        with_default(sub, || {
            record_nubi_request("POST", "503", "send");
        });
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].message, "nubi.request");
        assert_eq!(find_field(&captured[0].fields, "method"), Some("POST"));
        assert_eq!(find_field(&captured[0].fields, "status"), Some("503"));
        assert_eq!(find_field(&captured[0].fields, "endpoint"), Some("send"));
    }

    #[test]
    fn record_nubi_request_transport_error_label() {
        let (sub, events) = capture_subscriber();
        with_default(sub, || {
            record_nubi_request("POST", "transport_error", "send");
        });
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            find_field(&captured[0].fields, "status"),
            Some("transport_error")
        );
    }
}
