use crate::common::processed_payload_start_time;
use crate::db::InjectionRecord;
use crate::fpga::Device;
use crate::{capture::Stats, common::BLOCK_TIMEOUT};
use actix_web::{dev::Server, get, App, HttpResponse, HttpServer, Responder};
use paste::paste;
use prometheus::{
    register_gauge, register_gauge_vec, register_int_gauge, Gauge, GaugeVec, IntGauge, TextEncoder,
};
use rusqlite::Connection;
use std::sync::{
    mpsc::{Receiver, RecvTimeoutError},
    OnceLock,
};
use tokio::sync::broadcast;
use tracing::{info, warn};
use tracing_actix_web::TracingLogger;

const MONITOR_ACCUMULATIONS: u32 = 1048576; // Around 8 second at 8.192us

macro_rules! static_prom {
    ($name:ident, $kind: ty, $create:expr) => {
        paste! {
            fn $name() -> &'static $kind {
                static [<$name:upper>]: OnceLock<$kind> = OnceLock::new();
                [<$name:upper>].get_or_init(|| { $create })
            }
        }
    };
}

// Global prometheus state variables
static_prom!(
    spectrum_gauge,
    GaugeVec,
    register_gauge_vec!(
        "spectrum",
        "Average spectrum data",
        &["channel", "polarization"]
    )
    .unwrap()
);
static_prom!(
    packet_gauge,
    IntGauge,
    register_int_gauge!("processed_packets", "Number of packets we've processed").unwrap()
);
static_prom!(
    drop_gauge,
    IntGauge,
    register_int_gauge!("dropped_packets", "Number of packets we've dropped").unwrap()
);
static_prom!(
    shuffled_gauge,
    IntGauge,
    register_int_gauge!(
        "shuffled_packets",
        "Number of packets that were out of order"
    )
    .unwrap()
);
static_prom!(
    fft_ovlf_gauge,
    IntGauge,
    register_int_gauge!("fft_ovfl", "Counter of FFT overflows").unwrap()
);
static_prom!(
    fpga_temp,
    Gauge,
    register_gauge!("fpga_temp", "Internal FPGA temperature").unwrap()
);
static_prom!(
    adc_rms_gauge,
    GaugeVec,
    register_gauge_vec!("adc_rms", "RMS value of raw adc values", &["channel"]).unwrap()
);

#[get("/metrics")]
async fn metrics() -> impl Responder {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    HttpResponse::Ok().body(encoder.encode_to_string(&metric_families).unwrap())
}

#[get("/start_time")]
async fn start_time() -> impl Responder {
    let time = processed_payload_start_time();
    HttpResponse::Ok().body(time.to_mjd_tai_days().to_string())
}

fn update_spec(device: &mut Device) -> eyre::Result<()> {
    // Capture the spectrum
    let (a, b, stokes) = device.perform_both_vacc(MONITOR_ACCUMULATIONS)?;
    // And find the mean by dividing by N (and u32 max) to get 0-1
    let a_norm: Vec<_> = a
        .into_iter()
        .map(|x| x as f64 / (MONITOR_ACCUMULATIONS as f64 * u32::MAX as f64))
        .collect();
    let b_norm: Vec<_> = b
        .into_iter()
        .map(|x| x as f64 / (MONITOR_ACCUMULATIONS as f64 * u32::MAX as f64))
        .collect();
    let stokes_norm: Vec<_> = stokes
        .into_iter()
        .map(|x| x as f64 / (MONITOR_ACCUMULATIONS as f64 * u16::MAX as f64))
        .collect();
    // Finally update the gauge
    for (i, v) in a_norm.iter().enumerate() {
        spectrum_gauge()
            .with_label_values(&[&i.to_string(), "a"])
            .set(*v);
    }
    for (i, v) in b_norm.iter().enumerate() {
        spectrum_gauge()
            .with_label_values(&[&i.to_string(), "b"])
            .set(*v);
    }
    for (i, v) in stokes_norm.iter().enumerate() {
        spectrum_gauge()
            .with_label_values(&[&i.to_string(), "stokes"])
            .set(*v);
    }
    Ok(())
}

/// The monitor task publishes updates about the capture statistics, queries FPGA state, and updates the SQLite database on events
pub fn monitor_task(
    mut device: Device,
    capture_stats: Receiver<Stats>,
    injection_events: Receiver<InjectionRecord>,
    conn: Connection,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting monitoring task!");
    loop {
        // Look for shutdown signal
        if shutdown.try_recv().is_ok() {
            info!("Monitoring task stopping");
            break;
        }

        // If there's a new injection event, process that DB action
        if let Ok(r) = injection_events.try_recv() {
            match r.db_insert(&conn) {
                Ok(_) => (),
                Err(e) => warn!("Error processing DB event - {}", e),
            }
        }

        // Blocking here is ok, these are infrequent events
        match capture_stats.recv_timeout(BLOCK_TIMEOUT) {
            Ok(stat) => {
                packet_gauge().set(stat.processed.try_into().unwrap());
                drop_gauge().set(stat.drops.try_into().unwrap());
                shuffled_gauge().set(stat.shuffled.try_into().unwrap());
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Update channel data from FPGA
        match update_spec(&mut device) {
            Ok(_) => (),
            Err(e) => warn!("SNAP Error - {e}"),
        }

        // Metrics from the FPGA
        match device.fpga.fft_overflow_cnt.read() {
            Ok(v) => fft_ovlf_gauge().set(u32::from(v).into()),
            Err(e) => warn!("SNAP Error - {e}, {:?}", e),
        }

        match device.fpga.transport.lock().unwrap().temperature() {
            Ok(v) => fpga_temp().set(v.into()),
            Err(e) => warn!("SNAP Error - {e}, {:?}", e),
        }

        // Take a snapshot of ADC values and compute RMS value
        if device.fpga.adc_snap.arm().is_ok() && device.fpga.adc_snap.trigger().is_ok() {
            match device.fpga.adc_snap.read() {
                Ok(v) => {
                    let mut rms_a = 0.0;
                    let mut rms_b = 0.0;
                    let mut n = 0;
                    for chunk in v.chunks(4) {
                        rms_a += f64::powi(f64::from(chunk[0] as i8), 2);
                        rms_a += f64::powi(f64::from(chunk[1] as i8), 2);
                        rms_b += f64::powi(f64::from(chunk[2] as i8), 2);
                        rms_b += f64::powi(f64::from(chunk[3] as i8), 2);
                        n += 2;
                    }
                    rms_a = ((1.0 / (n as f64)) * rms_a).sqrt();
                    rms_b = ((1.0 / (n as f64)) * rms_b).sqrt();
                    adc_rms_gauge().with_label_values(&["a"]).set(rms_a);
                    adc_rms_gauge().with_label_values(&["b"]).set(rms_b);
                }
                Err(e) => warn!("SNAP Error - {e}, {:?}", e),
            }
        }
    }
    Ok(())
}

pub fn start_web_server(metrics_port: u16) -> eyre::Result<Server> {
    info!("Starting metrics webserver");
    // Create the server coroutine
    let server = HttpServer::new(move || {
        App::new()
            .wrap(TracingLogger::default()) // Tracing middleware
            .service(metrics)
            .service(start_time)
    })
    .bind(("0.0.0.0", metrics_port))?
    .workers(1)
    .run();
    // And return the coroutine for the caller to spawn
    Ok(server)
}
