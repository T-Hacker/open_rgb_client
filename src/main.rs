mod color_manager;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(target_os = "windows"))]
mod linux;

extern crate nvml_wrapper as nvml;

#[cfg(not(target_os = "windows"))]
use crate::linux::{install_service, remove_service, start_service};

use crate::color_manager::set_all_light_color;
use anyhow::Result;
use cpu_monitor::CpuInstant;
use log::*;
use nvml::Device;
use openrgb::{data::Color, OpenRGB};
use ringbuffer::{AllocRingBuffer, RingBuffer, RingBufferExt, RingBufferWrite};
use simplelog::{
    ColorChoice, CombinedLogger, Config, SharedLogger, TermLogger, TerminalMode, WriteLogger,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpStream, sync::Notify};

const SAMPLE_TIME: f32 = 5.0; // seconds.
const SAMPLE_RATE: u64 = 500;
const SAMPLE_BUFFER_SIZE: usize = (SAMPLE_TIME * (1.0 + 1.0 / SAMPLE_RATE as f32)) as usize;

const LOG_FILE: &str = "open_rgb_client_log.txt";

struct ShutdownSignal {
    shutdown_notify: Arc<Notify>,
    should_shutdown: AtomicBool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<String>>();

    // Setup logging.
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![];

    let in_service_mode = args.len() > 1 && args[1].eq_ignore_ascii_case("--service");
    if !in_service_mode {
        loggers.push(TermLogger::new(
            LevelFilter::Info,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ));
    }

    if cfg!(debug_assertions) {
        if let Ok(file) = std::fs::File::create(LOG_FILE) {
            loggers.push(WriteLogger::new(LevelFilter::Info, Config::default(), file));
        }
    }

    CombinedLogger::init(loggers).unwrap();

    log_panics::init();

    // Parse arguments.
    if args.len() > 1 {
        match args[1].as_str() {
            "--install" => install_service().await?,
            "--remove" => remove_service().await?,
            "--service" => {
                let exe_path = std::env::current_exe().unwrap();
                info!("Service is starting... [{:?}]", exe_path);

                start_service();

                info!("Service quit.");
            }

            _ => { /* Do nothing. */ }
        };
    } else {
        launch_client(None).await?;
    }

    info!("Done.");

    Ok(())
}

async fn launch_client(shutdown_signal: Option<Arc<ShutdownSignal>>) -> Result<()> {
    loop {
        info!("Connecting to OpenRGB...");

        let client = loop {
            let client = if let Some(shutdown_signal) = &shutdown_signal {
                if shutdown_signal.should_shutdown.load(Ordering::Relaxed) {
                    return Ok(());
                }

                tokio::select! {
                    _ = shutdown_signal.shutdown_notify.notified() => continue,
                    client = OpenRGB::connect() => client,
                }
            } else {
                OpenRGB::connect().await
            };

            if let Ok(client) = client {
                info!("Connected.");

                break client;
            } else {
                warn!("Failed to connect to OpenRGB. Retrying...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };

        info!("Initializing GPU monitoring...");
        let nvml = nvml::Nvml::init()?;
        let device = nvml.device_by_index(0)?;

        let sample_buffer_size = next_power_of_two(SAMPLE_BUFFER_SIZE as u32) as usize;
        let mut cpu_samples = AllocRingBuffer::with_capacity(sample_buffer_size);
        let mut gpu_samples = AllocRingBuffer::with_capacity(sample_buffer_size);

        if let Some(shutdown_signal) = &shutdown_signal {
            info!("Starting service loop...");

            loop {
                if shutdown_signal.should_shutdown.load(Ordering::Relaxed) {
                    return Ok(());
                }

                tokio::select! {
                    _ = shutdown_signal.shutdown_notify.notified() => continue,
                    result = sample_and_set(&client, &device, &mut cpu_samples, &mut gpu_samples) => {
                        if let Err(e) = result {
                            error!("Failed to sample and set: {}", e);

                            break;
                        }
                    }
                }
            }
        } else {
            info!("Starting normal process loop...");

            loop {
                if let Err(e) =
                    sample_and_set(&client, &device, &mut cpu_samples, &mut gpu_samples).await
                {
                    error!("Failed to sample and set: {}", e);

                    break;
                }
            }
        }
    }
}

async fn sample_and_set<'nvml>(
    client: &OpenRGB<TcpStream>,
    device: &Device<'nvml>,
    cpu_samples: &mut AllocRingBuffer<f32>,
    gpu_samples: &mut AllocRingBuffer<f32>,
) -> Result<()> {
    // CPU utilization.
    let start = CpuInstant::now()?;
    std::thread::sleep(Duration::from_millis(SAMPLE_RATE));
    let end = CpuInstant::now()?;
    let duration = end - start;
    let cpu_usage = duration.non_idle() as f32;
    cpu_samples.push(cpu_usage);

    let cpu_usage = cpu_samples
        .iter()
        .copied()
        .reduce(|accum, sample| accum + sample)
        .unwrap_or_default();
    let cpu_usage = cpu_usage / cpu_samples.len() as f32;

    // GPU utilization.
    let utilization = device.utilization_rates()?;
    let gpu_usage = utilization.gpu as f32 / 100.0;
    gpu_samples.push(gpu_usage);

    let gpu_usage = gpu_samples
        .iter()
        .copied()
        .reduce(|accum, sample| accum + sample)
        .unwrap_or_default();
    let gpu_usage = gpu_usage / gpu_samples.len() as f32;

    set_all_light_color(
        client,
        cpu_usage,
        gpu_usage,
        &Color::new(0xFF, 0xFF, 0xFF),
        &Color::new(0xFF, 0x0, 0x0),
    )
    .await?;

    info!("CPU: {} GPU: {}", cpu_usage, gpu_usage);

    tokio::task::yield_now().await;

    Ok(())
}

fn next_power_of_two(mut value: u32) -> u32 {
    value -= 1;
    value |= value >> 1;
    value |= value >> 2;
    value |= value >> 4;
    value |= value >> 8;
    value |= value >> 16;
    value += 1;

    value
}
