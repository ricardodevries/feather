use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use feather_core::ipc::{Command, send_request};
use feather_core::types::{DaemonStatus, DeviceDescriptor, Health};
use serde::Serialize;

use crate::{HEALTH_CHECK_FAILED, decode_payload, format_status_value, print_json};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DoctorResult {
    Pass,
    Warning,
    Fail,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DoctorCheck {
    name: String,
    result: DoctorResult,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct DoctorReport {
    version: String,
    socket: String,
    healthy: bool,
    checks: Vec<DoctorCheck>,
}

pub(crate) async fn run(socket: &Path, json: bool) -> Result<ExitCode> {
    let mut checks = Vec::new();
    match std::fs::metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            let mode = metadata.permissions().mode() & 0o777;
            let result = if mode & 0o007 != 0 {
                DoctorResult::Fail
            } else if mode == 0o660 {
                DoctorResult::Pass
            } else {
                DoctorResult::Warning
            };
            checks.push(DoctorCheck {
                name: "socket".into(),
                result,
                message: format!("{} is a Unix socket with mode {mode:04o}", socket.display()),
            });
        }
        Ok(_) => checks.push(DoctorCheck {
            name: "socket".into(),
            result: DoctorResult::Fail,
            message: format!("{} exists but is not a Unix socket", socket.display()),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "socket".into(),
            result: DoctorResult::Fail,
            message: format!("could not inspect {}: {error}", socket.display()),
        }),
    }

    match send_request(socket, Command::Status).await {
        Ok(data) => match decode_payload::<DaemonStatus>(data, "status") {
            Ok(status) => add_status_checks(&mut checks, &status),
            Err(error) => checks.push(DoctorCheck {
                name: "daemon".into(),
                result: DoctorResult::Fail,
                message: error.to_string(),
            }),
        },
        Err(error) => checks.push(DoctorCheck {
            name: "daemon".into(),
            result: DoctorResult::Fail,
            message: error.to_string(),
        }),
    }

    match send_request(socket, Command::Devices).await {
        Ok(data) => match decode_payload::<Vec<DeviceDescriptor>>(data, "device discovery") {
            Ok(devices) if devices.is_empty() => checks.push(DoctorCheck {
                name: "devices".into(),
                result: DoctorResult::Fail,
                message: "no supported hardware was detected".into(),
            }),
            Ok(devices) => checks.push(DoctorCheck {
                name: "devices".into(),
                result: DoctorResult::Pass,
                message: format!("{} supported devices detected", devices.len()),
            }),
            Err(error) => checks.push(DoctorCheck {
                name: "devices".into(),
                result: DoctorResult::Fail,
                message: error.to_string(),
            }),
        },
        Err(error) => checks.push(DoctorCheck {
            name: "devices".into(),
            result: DoctorResult::Fail,
            message: error.to_string(),
        }),
    }

    let healthy = checks
        .iter()
        .all(|check| check.result != DoctorResult::Fail);
    let report = DoctorReport {
        version: env!("CARGO_PKG_VERSION").into(),
        socket: socket.display().to_string(),
        healthy,
        checks,
    };
    if json {
        print_json(&serde_json::to_value(&report)?)?;
    } else {
        print_report(&report);
    }
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(HEALTH_CHECK_FAILED)
    })
}

fn add_status_checks(checks: &mut Vec<DoctorCheck>, status: &DaemonStatus) {
    let expected_version = env!("CARGO_PKG_VERSION");
    checks.push(DoctorCheck {
        name: "daemon".into(),
        result: if status.version == expected_version {
            DoctorResult::Pass
        } else {
            DoctorResult::Fail
        },
        message: if status.version == expected_version {
            format!(
                "featherd {} answered through the local socket",
                status.version
            )
        } else {
            format!(
                "client version {expected_version} does not match featherd {}",
                status.version
            )
        },
    });
    checks.push(DoctorCheck {
        name: "health".into(),
        result: result_for_health(&status.health),
        message: format!("daemon health is {:?}", status.health),
    });
    for (name, sensor) in &status.sensors {
        let reading = sensor.reading.as_ref().map_or_else(
            || "no current reading".into(),
            |reading| format!("{:.1} C, {} ms old", reading.celsius, reading.age_ms),
        );
        let error = sensor
            .error
            .as_ref()
            .map(|error| format!("; {error}"))
            .unwrap_or_default();
        checks.push(DoctorCheck {
            name: format!("sensor.{name}"),
            result: result_for_health(&sensor.health),
            message: format!("{reading}; health={:?}{error}", sensor.health),
        });
    }
    for (name, output) in &status.outputs {
        let error = output
            .error
            .as_ref()
            .map(|error| format!("; {error}"))
            .unwrap_or_default();
        checks.push(DoctorCheck {
            name: format!("output.{name}"),
            result: result_for_health(&output.health),
            message: format!(
                "{:?} target={}; health={:?}{error}",
                output.kind,
                format_status_value(output.target.as_ref()),
                output.health
            ),
        });
    }
    if !status.overrides.is_empty() {
        checks.push(DoctorCheck {
            name: "overrides".into(),
            result: DoctorResult::Warning,
            message: format!("{} temporary overrides are active", status.overrides.len()),
        });
    }
    for (name, error) in &status.driver_errors {
        checks.push(DoctorCheck {
            name: format!("driver.{name}"),
            result: DoctorResult::Fail,
            message: error.clone(),
        });
    }
}

fn result_for_health(health: &Health) -> DoctorResult {
    match health {
        Health::Healthy => DoctorResult::Pass,
        Health::Released => DoctorResult::Warning,
        Health::Degraded | Health::FailSafe | Health::Unavailable => DoctorResult::Fail,
    }
}

fn print_report(report: &DoctorReport) {
    println!("Feather doctor {}", report.version);
    for check in &report.checks {
        let result = match check.result {
            DoctorResult::Pass => "pass",
            DoctorResult::Warning => "warn",
            DoctorResult::Fail => "fail",
        };
        println!("  [{result}] {}: {}", check.name, check.message);
    }
    if report.healthy {
        println!("Result: healthy");
    } else {
        println!("Result: problems found");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_health_results() {
        assert_eq!(result_for_health(&Health::Healthy), DoctorResult::Pass);
        assert_eq!(result_for_health(&Health::Released), DoctorResult::Warning);
        assert_eq!(result_for_health(&Health::Degraded), DoctorResult::Fail);
        assert_eq!(result_for_health(&Health::FailSafe), DoctorResult::Fail);
        assert_eq!(result_for_health(&Health::Unavailable), DoctorResult::Fail);
    }
}
