pub use clap::Parser;
use grex_t0::{args, pipeline::start_pipeline, telemetry::init_tracing_subscriber};

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    // Setup the error handler
    color_eyre::install()?;
    // Get the CLI options
    let cli = args::Cli::parse();
    // Setup telemetry (logs, spans, traces, eventually metrics)
    let _guard = init_tracing_subscriber().await;
    // Spawn all the tasks and return the handles
    let handles = start_pipeline(cli).await?;
    // Join them all when we kill the task
    for handle in handles {
        handle.join().unwrap()?;
    }
    // Cleanup logging
    opentelemetry::global::shutdown_tracer_provider();
    Ok(())
}
