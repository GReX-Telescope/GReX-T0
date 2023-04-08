use anyhow::bail;
pub use clap::Parser;
use core_affinity::CoreId;
use crossbeam_channel::bounded;
use grex_t0::{
    args, capture,
    dumps::{self, DumpRing},
    exfil,
    fpga::Device,
    monitoring,
};
use log::{info, LevelFilter};
use rsntp::SntpClient;

use tokio::try_join;

// Setup the static channels
const FAST_PATH_CHANNEL_SIZE: usize = 1024;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Get the CLI options
    let cli = args::Cli::parse();
    // Get the CPU core range
    let mut cpus = cli.core_range;
    // Logger init
    pretty_env_logger::formatted_builder()
        .filter_level(LevelFilter::Info)
        .init();
    // Setup NTP
    let time_sync = if !cli.skip_ntp {
        // Setup NTP
        info!("Synchronizing time with NTP");
        let client = SntpClient::new();
        Some(client.synchronize(cli.ntp_addr).unwrap())
    } else {
        None
    };
    // Setup the FPGA
    info!("Setting up SNAP");
    let mut device = Device::new(cli.fpga_addr, cli.requant_gain);
    device.reset()?;
    device.start_networking()?;
    let packet_start = if !cli.skip_ntp {
        device.trigger(&time_sync.unwrap())?
    } else {
        device.blind_trigger()?
    };
    // Create a clone of the packet start time to hand off to the other thread
    let psc = packet_start;
    if cli.trig {
        device.force_pps();
    }
    // Create the dump ring
    let ring = DumpRing::new(cli.vbuf_power);

    // Fast path channels
    let (cap_s, cap_r) = bounded(FAST_PATH_CHANNEL_SIZE);

    // Less important channels, these don't have to be static
    let (trig_s, trig_r) = bounded(5);
    let (stat_s, stat_r) = bounded(100);

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
        ("capture", capture::cap_task(cli.cap_port, cap_s, stat_s)),
        ("consume", exfil::dummy_consumer(cap_r))
    );

    let _ = try_join!(
        // Start the webserver
        tokio::spawn(monitoring::start_web_server(cli.metrics_port)),
        // Start the trigger watch
        tokio::spawn(dumps::trigger_task(trig_s, cli.trig_port))
    )?;

    // Join them all when we kill the task
    for handle in handles {
        handle.join().unwrap()?;
    }

    Ok(())
}
