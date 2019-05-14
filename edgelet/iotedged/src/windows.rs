// Copyright (c) Microsoft. All rights reserved.

use std::cell::RefCell;
use std::env;
use std::ffi::OsString;
use std::time::Duration;

use clap::crate_name;
use failure::ResultExt;
use futures::future::Future;
use log::{error, info};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    register, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};

use crate::app;
use crate::error::{Error, ErrorKind, InitializeErrorReason, ServiceError};
use crate::logging;
use crate::signal;

const RUN_AS_CONSOLE_KEY: &str = "IOTEDGE_RUN_AS_CONSOLE";
const IOTEDGED_SERVICE_NAME: &str = crate_name!();

define_windows_service!(ffi_service_main, iotedge_service_main);

fn iotedge_service_main(args: Vec<OsString>) {
    match run_as_service(args) {
        Ok(status_handle) => {
            // Graceful shutdown
            info!("Stopping {} service...", IOTEDGED_SERVICE_NAME);
            update_service_state(status_handle, ServiceState::Stopped).unwrap();
            info!("Stopped {} service.", IOTEDGED_SERVICE_NAME);
        }

        Err(err) => {
            error!("Error while running service. Quitting.");
            logging::log_error(&err);
            std::process::exit(1);
        }
    }
}

fn run_as_service(_: Vec<OsString>) -> Result<ServiceStatusHandle, Error> {
    // Configure a Signal Future to alert us if Windows shuts us down.
    let windows_signal = signal_future::signal();
    let ws_signaler = RefCell::new(windows_signal.clone());

    // setup the service control handler
    let status_handle = register(
        IOTEDGED_SERVICE_NAME,
        move |control_event| match control_event {
            ServiceControl::Shutdown | ServiceControl::Stop => {
                info!("{} service is shutting down", IOTEDGED_SERVICE_NAME);

                ws_signaler.borrow_mut().signal();

                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        },
    )
    .map_err(ServiceError::from)
    .context(ErrorKind::Initialize(
        InitializeErrorReason::RegisterWindowsService,
    ))?;

    // initialize iotedged
    info!("Initializing {} service.", IOTEDGED_SERVICE_NAME);
    let settings = app::init_win_svc()?;
    let main = super::Main::new(settings);

    // tell Windows we're all set
    update_service_state(status_handle, ServiceState::Running)?;

    // start running
    info!("Starting {} service.", IOTEDGED_SERVICE_NAME);
    main.run_until(move || {
        Box::new(
            signal::shutdown()
                .select(windows_signal.clone())
                .map(move |_| {
                    info!("Stopping {} service.", IOTEDGED_SERVICE_NAME);
                    if let Err(err) = update_service_state(status_handle, ServiceState::StopPending)
                    {
                        error!(
                            "An error occurred while setting service status to STOP_PENDING: {:?}",
                            err,
                        );
                    }
                })
                .map_err(|_| ()),
        )
    })?;

    Ok(status_handle)
}

pub fn run_as_console() -> Result<(), Error> {
    let settings = app::init()?;
    let main = super::Main::new(settings);

    main.run_until(move || signal::shutdown())?;
    
    Ok(())
}

pub fn run() -> Result<(), Error> {
    // start app as a console app if an environment variable called
    // IOTEDGE_RUN_AS_CONSOLE exists
    if env::var(RUN_AS_CONSOLE_KEY).is_ok() {
        run_as_console()?;
        Ok(())
    } else {
        // kick-off the Windows service dance
        service_dispatcher::start(IOTEDGED_SERVICE_NAME, ffi_service_main)
            .map_err(ServiceError::from)
            .context(ErrorKind::Initialize(
                InitializeErrorReason::StartWindowsService,
            ))?;
        Ok(())
    }
}

fn update_service_state(
    status_handle: ServiceStatusHandle,
    current_state: ServiceState,
) -> Result<(), Error> {
    status_handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OwnProcess,
            current_state,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
        })
        .context(ErrorKind::UpdateWindowsServiceState)?;
    Ok(())
}
