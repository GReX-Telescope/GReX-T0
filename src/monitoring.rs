use crate::capture::Stats;
use crate::common::Stokes;
use crate::fpga::Device;
use crossbeam_channel::Receiver;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_TYPE;
use hyper::http::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use lazy_static::lazy_static;
use log::{error, info, warn};
use prometheus::{
    register_gauge, register_gauge_vec, register_int_gauge, register_int_gauge_vec, Encoder, Gauge,
    GaugeVec, IntGauge, IntGaugeVec, TextEncoder,
};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;

lazy_static! {
    static ref CHANNEL_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "task_channel_backlog",
        "Number of yet-to-be-processed data in each inter-task channel",
        &["target_channel"]
    )
    .unwrap();
    static ref SPECTRUM_GAUGE: GaugeVec =
        register_gauge_vec!("spectrum", "Average spectrum data", &["channel"]).unwrap();
    static ref PACKET_GAUGE: IntGauge =
        register_int_gauge!("processed_packets", "Number of packets we've processed").unwrap();
    static ref DROP_GAUGE: IntGauge =
        register_int_gauge!("dropped_packets", "Number of packets we've dropped").unwrap();
    static ref FFT_OVFL_GAUGE: IntGauge =
        register_int_gauge!("fft_ovfl", "Counter of FFT overflows").unwrap();
    static ref REQUANT_OVFL_GAUGE: IntGaugeVec = register_int_gauge_vec!(
        "requant_ovfl",
        "Counter of requantization overflows",
        &["polarization"]
    )
    .unwrap();
    static ref FPGA_TEMP: Gauge =
        register_gauge!("fpga_temp", "Internal FPGA temperature").unwrap();
    static ref ADC_RMS_GAUGE: GaugeVec =
        register_gauge_vec!("adc_rms", "RMS value of raw adc values", &["channel"]).unwrap();
}

pub async fn metrics(
    _: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let body_str = encoder.encode_to_string(&metric_families).unwrap();
    let mut resp = Response::new(Full::new(body_str.into()));
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(encoder.format_type()).unwrap(),
    );
    Ok(resp)
}

pub fn monitor_task(
    device: Device,
    stats: Receiver<Stats>,
    avg: Receiver<Stokes>,
) -> anyhow::Result<()> {
    info!("Starting monitoring task!");
    loop {
        // Blocking here is ok, these are infrequent events
        let stat = stats.recv()?;
        PACKET_GAUGE.set(stat.processed.try_into().unwrap());
        DROP_GAUGE.set(stat.drops.try_into().unwrap());

        // Update channel data
        let avg_spec = avg.recv()?;
        for (i, v) in avg_spec.iter().enumerate() {
            SPECTRUM_GAUGE
                .with_label_values(&[&i.to_string()])
                .set(f64::from(*v));
        }

        // Metrics from the FPGA
        if let Ok(v) = device.fpga.fft_overflow_cnt.read() {
            FFT_OVFL_GAUGE.set(u32::from(v).try_into().unwrap());
        } else {
            warn!("Error reading from FPGA");
        }
        if let Ok(v) = device.fpga.requant_a_overflow.read() {
            REQUANT_OVFL_GAUGE
                .with_label_values(&["a"])
                .set(u32::from(v).try_into().unwrap());
        } else {
            warn!("Error reading from FPGA");
        }
        if let Ok(v) = device.fpga.requant_b_overflow.read() {
            REQUANT_OVFL_GAUGE
                .with_label_values(&["b"])
                .set(u32::from(v).try_into().unwrap());
        } else {
            warn!("Error reading from FPGA");
        }
        if let Ok(v) = device.fpga.transport.lock().unwrap().temperature() {
            FPGA_TEMP.set(v.try_into().unwrap());
        } else {
            warn!("Error reading from FPGA");
        }

        // Take a snapshot of ADC values and compute RMS value
        if device.fpga.adc_snap.arm().is_ok() && device.fpga.adc_snap.trigger().is_ok() {
            if let Ok(v) = device.fpga.adc_snap.read() {
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
                ADC_RMS_GAUGE.with_label_values(&["a"]).set(rms_a);
                ADC_RMS_GAUGE.with_label_values(&["b"]).set(rms_b);
                println!("RMS A - {rms_a}");
                println!("RMS B - {rms_b}");
            } else {
                warn!("Error reading ADC snapshot");
            }
        }

        // Update metrics
        // CHANNEL_GAUGE
        //     .with_label_values(&["to_downsample"])
        //     .set(all_chans.to_downsample.len().try_into().unwrap());
        // CHANNEL_GAUGE
        //     .with_label_values(&["to_dump"])
        //     .set(all_chans.to_dump.len().try_into().unwrap());
        // CHANNEL_GAUGE
        //     .with_label_values(&["to_exfil"])
        //     .set(all_chans.to_exfil.len().try_into().unwrap());
        // CHANNEL_GAUGE
        //     .with_label_values(&["to_sort"])
        //     .set(all_chans.to_sort.len().try_into().unwrap());
    }
}

pub async fn start_web_server(metrics_port: u16) -> anyhow::Result<()> {
    info!("Starting metrics webserver");
    let addr = SocketAddr::from(([0, 0, 0, 0], metrics_port));
    let listener = TcpListener::bind(addr).await?;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                error!("TCP accept error: {}", e);
                break;
            }
        };
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(stream, service_fn(metrics))
                .await
            {
                println!("Error serving connection: {err:?}");
            }
        });
    }
    Ok(())
}
