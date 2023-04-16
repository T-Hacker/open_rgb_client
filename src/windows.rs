use log::info;
use std::ffi::OsString;
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

define_windows_service!(ffi_service_main, my_service_main);

pub fn start_service() {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
}

pub fn my_service_main(_arguments: Vec<OsString>) {
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

pub async fn install_service() -> Result<()> {
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

pub async fn remove_service() -> Result<()> {
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
