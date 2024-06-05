use clap::{Parser, Subcommand};
use regex::Regex;
use std::{net::SocketAddr, ops::RangeInclusive, path::PathBuf};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to save voltage dumps
    #[arg(long, default_value = ".")]
    pub dump_path: PathBuf,
    /// Path to save filterbanks
    #[arg(long, default_value = ".")]
    pub filterbank_path: PathBuf,
    /// CPU cores to which we'll build tasks. They should share a NUMA node.
    #[arg(long, default_value = "0:7", value_parser = parse_core_range)]
    pub core_range: RangeInclusive<usize>,
    /// MAC address of the interface which data comes in on (used in ARP)
    #[arg(long, value_parser=parse_mac)]
    pub mac: [u8; 6],
    /// Port which we expect packets to be directed to
    #[arg(long, default_value_t = 60000)]
    #[clap(value_parser = clap::value_parser!(u16).range(1..))]
    pub cap_port: u16,
    /// Port which we expect to receive trigger messages
    #[arg(long, default_value_t = 65432)]
    #[clap(value_parser = clap::value_parser!(u16).range(1..))]
    pub trig_port: u16,
    /// Port to respond to prometheus requests for metrics
    #[arg(long, default_value_t = 8083)]
    #[clap(value_parser = clap::value_parser!(u16).range(1..))]
    pub metrics_port: u16,
    /// Downsample power of 2, up to 9 (as that's the size of the capture window).
    #[clap(value_parser = clap::value_parser!(u32).range(1..=9))]
    #[arg(long, short, default_value_t = 2)]
    pub downsample_power: u32,
    /// Voltage buffer capacity, 30s default
    #[arg(long, short, default_value_t = 3662109)]
    pub vbuf_capacity: usize,
    /// Socket address of the SNAP Board
    #[arg(long, default_value = "192.168.0.3:69")]
    pub fpga_addr: SocketAddr,
    /// NTP server to synchronize against
    #[arg(long, default_value = "time.google.com")]
    pub ntp_addr: String,
    /// Requantization gain
    #[arg(long)]
    pub requant_gain: u16,
    /// Force a pps trigger
    #[arg(long)]
    pub trig: bool,
    /// Sync FPGA timing without NTP
    #[arg(long)]
    pub skip_ntp: bool,
    /// Pulse injection cadence (seconds)
    #[arg(short, long, default_value_t = 3600)]
    pub injection_cadence: u64,
    /// Path to .dat files for pulse injection
    #[arg(short, long, default_value = "./fake")]
    pub pulse_path: PathBuf,
    /// Exfil method - leaving this unspecified will not save stokes data
    #[command(subcommand)]
    pub exfil: Option<Exfil>,
}

#[derive(Debug, Subcommand)]
pub enum Exfil {
    /// Use PSRDADA for exfil
    Psrdada {
        /// Hex key
        #[clap(short, long, value_parser = valid_dada_key)]
        key: i32,
        /// Window size in number of time samples
        #[clap(short, long, default_value_t = 65536)]
        samples: usize,
    },
    Filterbank,
}

fn valid_dada_key(s: &str) -> Result<i32, String> {
    i32::from_str_radix(s, 16).map_err(|_| "Invalid hex literal".to_string())
}

pub fn parse_core_range(input: &str) -> Result<RangeInclusive<usize>, String> {
    let re = Regex::new(r"(\d+):(\d+)").unwrap();
    let cap = re.captures(input).unwrap();
    let start: usize = cap[1].parse().unwrap();
    let stop: usize = cap[2].parse().unwrap();
    if stop < start {
        return Err("Invalid CPU range".to_owned());
    }
    if stop - start + 1 < 8 {
        return Err("Not enough CPU cores".to_owned());
    }
    Ok(start..=stop)
}

pub fn parse_mac(input: &str) -> Result<[u8; 6], String> {
    // Accepting a MAC address in the usual way (hex separated by colon)
    let mut mac = [0u8; 6];
    let splits: Vec<_> = input.split(':').collect();
    if splits.len() != 6 {
        return Err("Malformed MAC address".to_owned());
    }
    for (i, octet) in splits.iter().enumerate() {
        mac[i] = u8::from_str_radix(octet, 16).map_err(|_| "Invalid MAC")?;
    }
    Ok(mac)
}
