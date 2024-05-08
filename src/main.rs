pub use clap::Parser;
use core_affinity::CoreId;
use eyre::bail;
use grex_t0::{
    args,
    calibrate::calibrate,
    capture,
    common::{payload_start_time, Payload, CHANNELS},
    dumps::{self, DumpRing},
    exfil,
    fpga::Device,
    injection, monitoring, processing,
};
use rsntp::SntpClient;
use std::time::Duration;
use thingbuf::mpsc::blocking::{channel, StaticChannel};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::broadcast,
    try_join,
};
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, prelude::*, EnvFilter};

// Setup the static channels
static CAPTURE_CHAN: StaticChannel<Payload, 16_384> = StaticChannel::new();
static INJECT_CHAN: StaticChannel<Payload, 1024> = StaticChannel::new();
static DUMP_CHAN: StaticChannel<Payload, 16_384> = StaticChannel::new();

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    // Setup the error handler
    color_eyre::install()?;
    // Get the CLI options
    let cli = args::Cli::parse();
    // Get the CPU core range
    let mut cpus = cli.core_range;
    // Create a new OpenTelemetry exporter
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic()) // gRPC exporter to localhost collector
        .install_batch(opentelemetry_sdk::runtime::TokioCurrentThread)?;
    // Create a tracing layer with the configured tracer
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    // Use the tracing subscriber `Registry`, or any other subscriber
    // that impls `LookupSpan`
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(telemetry)
        .with(EnvFilter::from_default_env())
        .init();
    // Create the dump ring (early in the program lifecycle to give it a chance to allocate)
    info!("Allocating RAM for the voltage ringbuffer!");
    let ring = DumpRing::new(cli.vbuf_power);
    // Setup the exit handler
    let (sd_s, sd_cap_r) = broadcast::channel(1);
    let sd_mon_r = sd_s.subscribe();
    let sd_inject_r = sd_s.subscribe();
    let sd_downsamp_r = sd_s.subscribe();
    let sd_dump_r = sd_s.subscribe();
    let sd_exfil_r = sd_s.subscribe();
    let sd_trig_r = sd_s.subscribe();
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).unwrap();
        let mut quit = signal(SignalKind::quit()).unwrap();
        let mut int = signal(SignalKind::interrupt()).unwrap();
        tokio::select! {
            _ = term.recv() => (),
            _ = quit.recv() => (),
            _ = int.recv() => (),
        }
        info!("Shutting down!");
        sd_s.send(()).unwrap()
    });
    // Setup NTP
    let time_sync = if !cli.skip_ntp {
        info!("Synchronizing time with NTP");
        let client = SntpClient::new();
        Some(client.synchronize(cli.ntp_addr).unwrap())
    } else {
        info!("Skipping NTP time sync");
        None
    };
    // Setup the FPGA
    info!("Setting up SNAP");
    let mut device = Device::new(cli.fpga_addr);
    device.reset()?;
    device.start_networking(&cli.mac)?;
    let packet_start = if !cli.skip_ntp {
        info!("Triggering the flow of packets via PPS");
        device.trigger(&time_sync.unwrap())?
    } else {
        info!("Blindly triggering (no GPS), timing will be off");
        device.blind_trigger()?
    };
    // Move this packet_start time into the global variable that everyone can use
    {
        // In our own little scope because we don't want to hold a non-async mutex across an
        // await boundary.
        info!(
            "Packet 0 is coincident with {} MJD (TAI)",
            packet_start.to_mjd_tai_days()
        );
        let mut ps = payload_start_time().lock().unwrap();
        *ps = Some(packet_start);
    }
    if cli.trig {
        device.force_pps()?;
    }
    // Perform the bandpass calibration routine (if needed)
    if let Some(requant_gain) = cli.requant_gain {
        info!("Setting requant gains directly without bandpass calibration");
        let gain = [requant_gain; CHANNELS];
        device.set_requant_gains(&gain, &gain)?;
    } else {
        info!("Calibrating bandpass");
        calibrate(&mut device)?;
    }
    // These may not need to be static
    let (cap_s, cap_r) = CAPTURE_CHAN.split();
    let (dump_s, dump_r) = DUMP_CHAN.split();
    let (inject_s, inject_r) = INJECT_CHAN.split();
    // Fast path channels
    let (ex_s, ex_r) = channel(1024);

    // Less important channels, these don't have to be static (and we don't need thingbuf)
    let (trig_s, trig_r) = std::sync::mpsc::sync_channel(5);
    let (stat_s, stat_r) = std::sync::mpsc::sync_channel(100);

    // Start the threads
    macro_rules! thread_spawn {
            ($(($thread_name:literal, $fcall:expr)), +) => {
                  vec![$({let cpu = cpus.next().unwrap();
                    std::thread::Builder::new()
                        .name($thread_name.to_string())
                        .spawn( move || {
                            if !core_affinity::set_for_current(CoreId { id: cpu}) {
                                bail!("Couldn't set core affinity on thread {}", $thread_name);
                            }
                            $fcall
                        })
                        .unwrap()}),+]
            };
        }
    // Spawn all the threads
    let handles = thread_spawn!(
        (
            "collect",
            monitoring::monitor_task(device, stat_r, sd_mon_r)
        ),
        (
            "injection",
            injection::pulse_injection_task(
                cap_r,
                inject_s,
                Duration::from_secs(cli.injection_cadence),
                cli.pulse_path,
                sd_inject_r
            )
        ),
        (
            "downsample",
            processing::downsample_task(
                inject_r,
                ex_s,
                dump_s,
                cli.downsample_power,
                sd_downsamp_r
            )
        ),
        (
            "dump",
            dumps::dump_task(
                ring,
                dump_r,
                trig_r,
                cli.dump_path,
                cli.downsample_power,
                sd_dump_r
            )
        ),
        (
            "exfil",
            match cli.exfil {
                Some(e) => match e {
                    args::Exfil::Psrdada { key, samples } => exfil::dada_consumer(
                        key,
                        ex_r,
                        2usize.pow(cli.downsample_power),
                        samples,
                        sd_exfil_r
                    ),
                    args::Exfil::Filterbank => exfil::filterbank_consumer(
                        ex_r,
                        2usize.pow(cli.downsample_power),
                        &cli.filterbank_path,
                        sd_exfil_r
                    ),
                },
                None => exfil::dummy_consumer(ex_r, sd_exfil_r),
            }
        ),
        (
            "capture",
            capture::cap_task(cli.cap_port, cap_s, stat_s, sd_cap_r)
        )
    );

    let _ = try_join!(
        // Start the webserver
        tokio::spawn(monitoring::start_web_server(cli.metrics_port,)?),
        // Start the trigger watch
        tokio::spawn(dumps::trigger_task(trig_s, cli.trig_port, sd_trig_r))
    )?;

    // Join them all when we kill the task
    for handle in handles {
        handle.join().unwrap()?;
    }

    Ok(())
}
