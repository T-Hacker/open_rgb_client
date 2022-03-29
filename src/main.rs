mod color_manager;

extern crate nvml_wrapper as nvml;

use crate::color_manager::set_all_light_color;
use anyhow::Result;
use cpu_monitor::CpuInstant;
use log::*;
use openrgb::{data::Color, OpenRGB};
use ringbuffer::{AllocRingBuffer, RingBuffer, RingBufferExt, RingBufferWrite};
use simplelog::{ColorChoice, CombinedLogger, Config, TermLogger, TerminalMode};
use std::time::Duration;

const SAMPLE_TIME: f32 = 5.0; // seconds.
const SAMPLE_RATE: u64 = 500;
const SAMPLE_BUFFER_SIZE: usize = (SAMPLE_TIME * (1.0 + 1.0 / SAMPLE_RATE as f32)) as usize;

#[tokio::main]
async fn main() -> Result<()> {
    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        // WriteLogger::new(
        //     LevelFilter::Info,
        //     Config::default(),
        //     File::create("my_rust_binary.log").unwrap(),
        // ),
    ])?;

    info!("Connecting to OpenRGB...");

    let client = loop {
        if let Ok(client) = OpenRGB::connect().await {
            break client;
        } else {
            warn!("Failed to connect to OpenRGB. Retrying...");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    info!("Connected.");

    let nvml = nvml::NVML::init()?;
    let device = nvml.device_by_index(0)?;

    let sample_buffer_size = next_power_of_two(SAMPLE_BUFFER_SIZE as u32) as usize;
    let mut cpu_samples = AllocRingBuffer::with_capacity(sample_buffer_size);
    let mut gpu_samples = AllocRingBuffer::with_capacity(sample_buffer_size);

    loop {
        // CPU utilization.
        let start = CpuInstant::now()?;
        std::thread::sleep(Duration::from_millis(SAMPLE_RATE));
        let end = CpuInstant::now()?;
        let duration = end - start;
        let cpu_usage = duration.non_idle() as f32;
        cpu_samples.push(cpu_usage);

        let cpu_usage = cpu_samples
            .iter()
            .map(|sample| *sample)
            .reduce(|accum, sample| accum + sample)
            .unwrap_or_default();
        let cpu_usage = cpu_usage / cpu_samples.len() as f32;

        // GPU utilization.
        let utilization = device.utilization_rates()?;
        let gpu_usage = utilization.gpu as f32 / 100.0;
        gpu_samples.push(gpu_usage);

        let gpu_usage = gpu_samples
            .iter()
            .map(|sample| *sample)
            .reduce(|accum, sample| accum + sample)
            .unwrap_or_default();
        let gpu_usage = gpu_usage / gpu_samples.len() as f32;

        // dbg!(cpu_usage, gpu_usage);

        set_all_light_color(
            &client,
            cpu_usage,
            gpu_usage,
            &Color::new(0xFF, 0xFF, 0xFF),
            &Color::new(0xFF, 0x0, 0x0),
        )
        .await?;
    }
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
