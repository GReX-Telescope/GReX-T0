use anyhow::bail;
pub use clap::Parser;
use core_affinity::CoreId;
use grex_t0::{
    args, capture,
    dumps::{self, DumpRing},
    exfil,
    fpga::Device,
    injection, monitoring, processing,
};
use jemallocator::Jemalloc;
use log::{info, LevelFilter};
use rsntp::SntpClient;
use thingbuf::mpsc::blocking::channel;
use tokio::try_join;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

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
    info!("Synchronizing time with NTP");
    let client = SntpClient::new();
    let time_sync = client.synchronize(cli.ntp_addr).unwrap();
    // Setup the FPGA
    info!("Setting up SNAP");
    let mut device = Device::new(cli.fpga_addr, cli.requant_gain);
    device.reset()?;
    device.start_networking()?;
    let packet_start = device.trigger(&time_sync)?;
    // Create a clone of the packet start time to hand off to the other thread
    let psc = packet_start;
    if cli.trig {
        device.force_pps();
    }
    // Create the dump ring
    let ring = DumpRing::new(cli.vbuf_power);
    // Create channels to connect everything
    let fast_path_buffers = 1024;
    let (pb_s, pb_r) = channel(fast_path_buffers);
    let (ds_s, ds_r) = channel(fast_path_buffers);
    let (ex_s, ex_r) = channel(fast_path_buffers);
    let (dump_s, dump_r) = channel(fast_path_buffers);
    let (split_s, split_r) = channel(fast_path_buffers);
    let (inject_s, inject_r) = channel(fast_path_buffers);
    let (trig_s, trig_r) = channel(5);
    let (stat_s, stat_r) = channel(100);
    let (avg_s, avg_r) = channel(100);

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
        ("collect", monitoring::monitor_task(device, stat_r, avg_r)),
        ("injection", injection::pulse_injection_task(inject_r, ex_s)),
        (
            "downsample",
            processing::downsample_task(ds_r, inject_s, avg_s, cli.downsample_power)
        ),
        ("decode", capture::decode_task(pb_r, split_s)),
        ("split", capture::split_task(split_r, ds_s, dump_s,)),
        ("dump", dumps::dump_task(ring, dump_r, trig_r, packet_start)),
        (
            "exfil",
            match cli.exfil {
                Some(e) => match e {
                    args::Exfil::Psrdada { key, samples } => exfil::dada_consumer(
                        key,
                        ex_r,
                        psc,
                        2usize.pow(cli.downsample_power),
                        samples
                    ),
                    args::Exfil::Filterbank => todo!(),
                },
                None => exfil::dummy_consumer(ex_r),
            }
        ),
        ("capture", capture::cap_task(cli.cap_port, pb_s, stat_s))
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
