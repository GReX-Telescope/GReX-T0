pub use clap::Parser;
use grex_t0::{args, pipeline::start_pipeline, telemetry::init_tracing_subscriber};
use tracing::span;

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    // Setup the error handler
    color_eyre::install()?;
    // Get the CLI options
    let cli = args::Cli::parse();
    // Setup telemetry (logs, spans, traces, eventually metrics)
    let _guard = init_tracing_subscriber().await;

    {
        // Create a root span for logging
        let root = span!(tracing::Level::INFO, "app_start");
        let _enter = root.enter();
        // Spawn all the tasks and return the handles
        let handles = start_pipeline(cli).await?;
        // Join them all when we kill the task
        for handle in handles {
            handle.join().unwrap()?;
        }
    } // Once this scope is closed, all spans inside are closed as well

    // Cleanup logging
    opentelemetry::global::shutdown_tracer_provider();

    Ok(())
}
