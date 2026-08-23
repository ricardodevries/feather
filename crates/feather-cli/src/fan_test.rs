use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use feather_core::ipc::{Command, send_request};
use feather_core::types::{DaemonStatus, OutputKind, OutputStatus};
use serde::Serialize;

use crate::{HEALTH_CHECK_FAILED, decode_payload, duration_ms, print_json};

const TARGET_TIMEOUT: Duration = Duration::from_secs(5);
const OVERRIDE_GRACE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const POLICY_RESTORE_DELAY: Duration = Duration::from_millis(1_100);

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct FanTestStep {
    percent: u8,
    median_rpm: u64,
    rpm: BTreeMap<String, u64>,
    low_channels: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct FanTestReport {
    output: String,
    minimum_relative_percent: u8,
    steps: Vec<FanTestStep>,
    warnings: Vec<String>,
    passed: bool,
    policy_restored: bool,
}

pub(crate) async fn run(
    socket: &Path,
    output: &str,
    steps: &[u8],
    hold: Duration,
    minimum_relative_percent: u8,
    json: bool,
) -> Result<ExitCode> {
    validate_options(steps, hold)?;
    let initial = request_status(socket).await?;
    let initial_output = initial
        .outputs
        .get(output)
        .ok_or_else(|| anyhow::anyhow!("unknown output '{output}'"))?;
    if initial_output.kind != OutputKind::Fan {
        anyhow::bail!("output '{output}' is not a fan");
    }
    if initial
        .overrides
        .iter()
        .any(|active| active.output == output)
    {
        anyhow::bail!("output '{output}' already has an active override");
    }
    rpm_readings(output, initial_output)?;

    let override_ttl = hold
        .checked_add(TARGET_TIMEOUT)
        .and_then(|duration| duration.checked_add(OVERRIDE_GRACE))
        .ok_or_else(|| anyhow::anyhow!("fan test hold time is too large"))?;
    let mut override_active = false;
    let test_result: Result<Vec<FanTestStep>> = async {
        let mut samples = Vec::with_capacity(steps.len());
        for percent in steps {
            progress(
                json,
                &format!("Testing {output} at {percent}% for {hold:?}"),
            );
            override_active = true;
            send_request(
                socket,
                Command::OverrideFan {
                    output: output.into(),
                    percent: *percent,
                    duration_ms: duration_ms(override_ttl)?,
                    allow_below_minimum: false,
                },
            )
            .await?;
            wait_for_target(socket, output, *percent).await?;
            wait_for_settle(hold).await?;
            let status = request_status(socket).await?;
            let output_status = status.outputs.get(output).ok_or_else(|| {
                anyhow::anyhow!("output '{output}' disappeared during the fan test")
            })?;
            let rpm = rpm_readings(output, output_status)?;
            samples.push(analyze_step(*percent, rpm, minimum_relative_percent)?);
        }
        Ok(samples)
    }
    .await;

    let cleanup_result = if override_active {
        clear_override(socket, output).await
    } else {
        Ok(())
    };
    let samples = match (test_result, cleanup_result) {
        (Ok(samples), Ok(())) => samples,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(cleanup_error)) => return Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => {
            anyhow::bail!("{error:#}; cleanup also failed: {cleanup_error:#}")
        }
    };

    let warnings = warnings(&samples, minimum_relative_percent);
    let passed = warnings.is_empty();
    let report = FanTestReport {
        output: output.into(),
        minimum_relative_percent,
        steps: samples,
        warnings,
        passed,
        policy_restored: true,
    };
    if json {
        print_json(&serde_json::to_value(&report)?)?;
    } else {
        print_report(&report);
    }
    Ok(if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(HEALTH_CHECK_FAILED)
    })
}

fn validate_options(steps: &[u8], hold: Duration) -> Result<()> {
    if !(2..=10).contains(&steps.len()) {
        anyhow::bail!("fan test requires 2 to 10 duty steps");
    }
    if steps.windows(2).any(|pair| pair[0] >= pair[1]) {
        anyhow::bail!("fan test steps must be unique and in ascending order");
    }
    if hold > Duration::from_secs(5 * 60) {
        anyhow::bail!("fan test hold time must not exceed 5m per step");
    }
    Ok(())
}

async fn request_status(socket: &Path) -> Result<DaemonStatus> {
    let data = send_request(socket, Command::Status).await?;
    decode_payload(data, "status")
}

async fn wait_for_target(socket: &Path, output: &str, percent: u8) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(TARGET_TIMEOUT)
        .ok_or_else(|| anyhow::anyhow!("fan test deadline overflowed"))?;
    loop {
        let status = request_status(socket).await?;
        let current = status
            .outputs
            .get(output)
            .ok_or_else(|| anyhow::anyhow!("output '{output}' disappeared during the fan test"))?;
        if current.target.as_ref().and_then(serde_json::Value::as_u64) == Some(u64::from(percent)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("output '{output}' did not reach {percent}% within 5s");
        }
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            signal = tokio::signal::ctrl_c() => {
                signal.context("could not listen for Ctrl+C")?;
                anyhow::bail!("fan test interrupted");
            }
        }
    }
}

async fn wait_for_settle(hold: Duration) -> Result<()> {
    tokio::select! {
        () = tokio::time::sleep(hold) => Ok(()),
        signal = tokio::signal::ctrl_c() => {
            signal.context("could not listen for Ctrl+C")?;
            anyhow::bail!("fan test interrupted");
        }
    }
}

async fn clear_override(socket: &Path, output: &str) -> Result<()> {
    let status = request_status(socket).await?;
    if !status
        .overrides
        .iter()
        .any(|active| active.output == output)
    {
        return Ok(());
    }
    send_request(
        socket,
        Command::OverrideClear {
            output: Some(output.into()),
        },
    )
    .await?;

    // featherd evaluates policy at least once per second. Wait for that tick so
    // the report reflects the configured target rather than the cleared override.
    tokio::time::sleep(POLICY_RESTORE_DELAY).await;
    let status = request_status(socket).await?;
    if status
        .overrides
        .iter()
        .any(|active| active.output == output)
    {
        anyhow::bail!("fan test override for '{output}' remained active after cleanup");
    }
    if status
        .outputs
        .get(output)
        .and_then(|status| status.target.as_ref())
        .is_none()
    {
        anyhow::bail!("output '{output}' had no policy target after fan test cleanup");
    }
    Ok(())
}

fn rpm_readings(output: &str, status: &OutputStatus) -> Result<BTreeMap<String, u64>> {
    let rpm = status
        .observed
        .as_ref()
        .and_then(|observed| observed.get("rpm"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "output '{output}' does not report per-channel RPM and cannot be performance tested"
            )
        })?;
    let readings = rpm
        .iter()
        .map(|(channel, value)| {
            value
                .as_u64()
                .map(|rpm| (channel.clone(), rpm))
                .ok_or_else(|| {
                    anyhow::anyhow!("output '{output}' returned invalid RPM for channel {channel}")
                })
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if readings.is_empty() {
        anyhow::bail!("output '{output}' reported no RPM readings");
    }
    Ok(readings)
}

fn analyze_step(
    percent: u8,
    rpm: BTreeMap<String, u64>,
    minimum_relative_percent: u8,
) -> Result<FanTestStep> {
    let mut sorted = rpm.values().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    let median_rpm = if sorted.len() % 2 == 0 {
        sorted[middle - 1] + (sorted[middle] - sorted[middle - 1]) / 2
    } else {
        sorted[middle]
    };
    if median_rpm == 0 {
        anyhow::bail!("all fans reported 0 RPM at {percent}%");
    }
    let low_channels = rpm
        .iter()
        .filter(|(_, value)| {
            value.saturating_mul(100)
                < median_rpm.saturating_mul(u64::from(minimum_relative_percent))
        })
        .map(|(channel, _)| channel.clone())
        .collect();
    Ok(FanTestStep {
        percent,
        median_rpm,
        rpm,
        low_channels,
    })
}

fn warnings(steps: &[FanTestStep], minimum_relative_percent: u8) -> Vec<String> {
    let mut warnings = Vec::new();
    for step in steps {
        for channel in &step.low_channels {
            if let Some(rpm) = step.rpm.get(channel) {
                warnings.push(format!(
                    "channel {channel} reported {rpm} RPM at {}%, below {minimum_relative_percent}% of the {} RPM median",
                    step.percent, step.median_rpm
                ));
            }
        }
    }
    if let (Some(first), Some(last)) = (steps.first(), steps.last())
        && last.percent.saturating_sub(first.percent) >= 20
    {
        for (channel, first_rpm) in &first.rpm {
            let Some(last_rpm) = last.rpm.get(channel) else {
                warnings.push(format!("channel {channel} disappeared during the fan test"));
                continue;
            };
            if last_rpm.saturating_mul(100) < first_rpm.saturating_mul(125) {
                warnings.push(format!(
                    "channel {channel} changed from {first_rpm} to {last_rpm} RPM between {}% and {}% duty",
                    first.percent, last.percent
                ));
            }
        }
    }
    warnings
}

fn progress(json: bool, message: &str) {
    if json {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

fn print_report(report: &FanTestReport) {
    println!("Fan test: {}", report.output);
    for step in &report.steps {
        let readings = step
            .rpm
            .iter()
            .map(|(channel, rpm)| format!("{channel}={rpm}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!(
            "  {}%  median={} RPM  {readings}",
            step.percent, step.median_rpm
        );
    }
    if report.warnings.is_empty() {
        println!("Result: pass");
    } else {
        println!("Warnings:");
        for warning in &report.warnings {
            println!("  {warning}");
        }
        println!("Result: attention needed");
    }
    println!("Normal policy restored.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_analyzes_samples() -> anyhow::Result<()> {
        assert!(validate_options(&[35, 50, 75, 100], Duration::from_secs(15)).is_ok());
        assert!(validate_options(&[35], Duration::from_secs(15)).is_err());
        assert!(validate_options(&[50, 50], Duration::from_secs(15)).is_err());
        assert!(validate_options(&[50, 40], Duration::from_secs(15)).is_err());

        let first = analyze_step(
            35,
            BTreeMap::from([("1".into(), 800), ("2".into(), 810), ("3".into(), 790)]),
            75,
        )?;
        assert_eq!(first.median_rpm, 800);
        assert!(first.low_channels.is_empty());

        let last = analyze_step(
            100,
            BTreeMap::from([("1".into(), 2_000), ("2".into(), 900), ("3".into(), 1_980)]),
            75,
        )?;
        assert_eq!(last.low_channels, vec!["2"]);
        let warnings = warnings(&[first, last], 75);
        assert!(warnings.iter().any(|warning| warning.contains("channel 2")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("810 to 900 RPM"))
        );
        Ok(())
    }
}
