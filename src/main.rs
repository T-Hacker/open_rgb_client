mod color_manager;

extern crate nvml_wrapper as nvml;

use crate::color_manager::set_all_light_color;
use anyhow::{bail, Result};
use cpu_monitor::CpuInstant;
use log::*;
use nvml::Device;
use openrgb::{data::Color, OpenRGB};
use ringbuffer::{AllocRingBuffer, RingBuffer, RingBufferExt, RingBufferWrite};
use simplelog::{
    ColorChoice, CombinedLogger, Config, SharedLogger, TermLogger, TerminalMode, WriteLogger,
};
use std::{
    ffi::{OsStr, OsString},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use tokio::{net::TcpStream, sync::Notify};
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{register, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const SERVICE_NAME: &str = "open_rgb_client";
const SERVICE_DISPLAY_NAME: &str = "Open RGB Client";
const SERVICE_DESCRIPTION: &str = "OpenRGB Client that changes light color based on system load.";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const SERVICE_ARGUMENTS: &[&OsStr] = &[];

const SAMPLE_TIME: f32 = 5.0; // seconds.
const SAMPLE_RATE: u64 = 500;
const SAMPLE_BUFFER_SIZE: usize = (SAMPLE_TIME * (1.0 + 1.0 / SAMPLE_RATE as f32)) as usize;

const LOG_FILE: &str = "open_rgb_client_log.txt";

struct ShutdownSignal {
    shutdown_notify: Arc<Notify>,
    should_shutdown: AtomicBool,
}

define_windows_service!(ffi_service_main, my_service_main);

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

                service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;

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

fn my_service_main(_arguments: Vec<OsString>) {
    info!("Service is running, initializing async runtime...");

    let threaded_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(windows_service::Error::Winapi)
        .unwrap();

    threaded_rt.block_on(async {
        info!("Async runtime is initialized.");

        // Create a shutdown notification to signal the main loop to stop accepting more connection.
        let shutdown_signal = Arc::new(ShutdownSignal {
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            should_shutdown: AtomicBool::new(false),
        });

        // Define system service event handler that will be receiving service events.
        let shutdown_signal_copy = shutdown_signal.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                // Notifies a service to report its current status information to the service
                // control manager. Always return NoError even if not implemented.
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,

                // Handle stop
                ServiceControl::Stop => {
                    shutdown_signal_copy
                        .should_shutdown
                        .store(true, Ordering::Relaxed);
                    shutdown_signal_copy.shutdown_notify.notify_waiters();

                    info!("Giving time to shutdown gracefully...");
                    thread::sleep(Duration::from_secs(1));

                    ServiceControlHandlerResult::NoError
                }

                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        // Register system service event handler.
        // The returned status handle should be used to report service status changes to the system.
        let status_handle = register(SERVICE_NAME, event_handler).unwrap();

        // Tell the system that service is running
        status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .unwrap();

        // Start main work loop.
        let exit_code = match launch_client(shutdown_signal.into()).await {
            Ok(_) => {
                info!("Stopping without errors.");

                0 // No error.
            }
            Err(e) => {
                error!("Exiting from loop with error: {}", e);

                std::process::exit(-1); // Exit with error to force Windows to restart the service.
            }
        };

        // Tell the system that service has stopped.
        info!("Service is stopping...");
        status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(exit_code),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .unwrap();
    });
}

async fn launch_client(shutdown_signal: Option<Arc<ShutdownSignal>>) -> Result<()> {
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
                        bail!("Failed to sample and set: {}", e);
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
                bail!("Failed to sample and set: {}", e);
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

async fn install_service() -> Result<()> {
    // First, try to remove the service.
    remove_service().await.unwrap_or_default();

    // Install service.
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_binary_path = ::std::env::current_exe().unwrap();

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: service_binary_path,
        launch_arguments: vec!["--service".into()],
        dependencies: vec![],
        account_name: None, // run as System
        account_password: None,
    };

    let service = service_manager.create_service(
        &service_info,
        ServiceAccess::START | ServiceAccess::CHANGE_CONFIG,
    )?;
    service.set_description(SERVICE_DESCRIPTION)?;
    service.start(SERVICE_ARGUMENTS)?;

    Ok(())
}

async fn remove_service() -> Result<()> {
    let manager_access = ServiceManagerAccess::CONNECT;
    let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = service_manager.open_service(SERVICE_NAME, service_access)?;

    let service_status = service.query_status()?;
    if service_status.current_state != ServiceState::Stopped {
        service.stop()?;

        // Wait for service to stop
        thread::sleep(Duration::from_secs(3));
    }

    service.delete()?;

    Ok(())
}
