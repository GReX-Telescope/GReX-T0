use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{
    runtime,
    trace::{BatchConfig, RandomIdGenerator, Sampler},
    Resource,
};
use opentelemetry_semantic_conventions::{
    resource::{DEPLOYMENT_ENVIRONMENT, SERVICE_NAME, SERVICE_VERSION},
    SCHEMA_URL,
};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Create a Resource that captures information about the entity for which telemetry is recorded.
fn resource() -> Resource {
    Resource::from_schema_url(
        [
            KeyValue::new(SERVICE_NAME, env!("CARGO_PKG_NAME")),
            KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
            KeyValue::new(DEPLOYMENT_ENVIRONMENT, "production"),
        ],
        SCHEMA_URL,
    )
}

/// Initialize tracing-subscriber
pub async fn init_tracing_subscriber() {
    let traces = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_trace_config(
            opentelemetry_sdk::trace::Config::default()
                // Customize sampling strategy
                .with_sampler(Sampler::AlwaysOn)
                // If export trace to AWS X-Ray, you can use XrayIdGenerator
                .with_id_generator(RandomIdGenerator::default())
                .with_resource(resource()),
        )
        .with_batch_config(BatchConfig::default())
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .install_batch(runtime::TokioCurrentThread)
        .expect("Could not create OpenTelemetry tracer");

    let logs = opentelemetry_otlp::new_pipeline()
        .logging()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .with_log_config(opentelemetry_sdk::logs::config().with_resource(resource()))
        .install_batch(opentelemetry_sdk::runtime::TokioCurrentThread)
        .expect("Could not create OpenTelemetry logger");

    let trace_layer = OpenTelemetryLayer::new(traces);
    let log_layer = OpenTelemetryTracingBridge::new(logs.provider());

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .with(trace_layer)
        .with(log_layer)
        .init();
}
