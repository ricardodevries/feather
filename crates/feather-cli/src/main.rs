use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use chrono::Local;
use chrono::Timelike;
use clap::CommandFactory;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use clap_complete::Shell;
use feather_core::config::{Config, OutputConfig};
use feather_core::curve::{interpolate_color, interpolate_fan, schedule_active};
use feather_core::error::FeatherError;
use feather_core::ipc::{Command, send_request};
use feather_core::types::{DaemonStatus, DeviceDescriptor, Health};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing_subscriber::EnvFilter;

const DEFAULT_SOCKET: &str = "/run/feather/feather.sock";
const HEALTH_CHECK_FAILED: u8 = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReloadPayload {
    generation: u64,
    active_profile: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesPayload {
    profiles: Vec<String>,
    active_profile: String,
    persisted_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveProfilePayload {
    active_profile: String,
    persisted: bool,
    persisted_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HidDumpPayload {
    device: String,
    dump: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "feather",
    version,
    about = "Control featherd through its local Unix socket"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "FEATHER_SOCKET",
        help = "Daemon socket (default: /run/feather/feather.sock)"
    )]
    socket: Option<PathBuf>,

    /// Increase log detail. Repeat for protocol-level logs.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Show daemon, sensor, and output state.
    Status {
        /// Emit the versioned status object as JSON.
        #[arg(long)]
        json: bool,
        /// Refresh continuously. The default interval is 2s.
        #[arg(
            long,
            value_name = "INTERVAL",
            num_args = 0..=1,
            default_missing_value = "2s",
            value_parser = parse_duration,
            conflicts_with = "check"
        )]
        watch: Option<Duration>,
        /// Exit with status 5 when daemon health is not healthy.
        #[arg(long)]
        check: bool,
    },
    /// List detected devices and temperature sensors.
    Devices {
        /// Emit device descriptors as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Validate or reload configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List or activate configured profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Apply or clear temporary output overrides.
    Override {
        #[command(subcommand)]
        command: OverrideCommand,
    },
    /// Release or resume daemon ownership of outputs.
    Output {
        #[command(subcommand)]
        command: OutputCommand,
    },
    /// Read hardware diagnostics without changing output values.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    /// Generate a shell completion script on stdout.
    Completions {
        /// Shell whose completion syntax should be generated.
        shell: CompletionShell,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
}

impl From<CompletionShell> for Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::PowerShell => Self::PowerShell,
            CompletionShell::Zsh => Self::Zsh,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate a TOML file without contacting the daemon.
    Check {
        /// TOML file to validate.
        path: PathBuf,
    },
    /// Show profile targets for one or more synthetic temperatures.
    Preview {
        /// TOML file to preview.
        path: PathBuf,
        /// Profile to preview. Defaults to `default_profile`.
        #[arg(long)]
        profile: Option<String>,
        /// Temperatures to evaluate. Curve points are used when omitted.
        #[arg(value_name = "CELSIUS", value_parser = parse_temperature)]
        temperatures: Vec<f64>,
    },
    /// Reload the daemon's configured TOML file.
    Reload,
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// List configured profiles.
    List,
    /// Activate a configured profile.
    Set {
        /// Configured profile name.
        profile: String,
        /// Save this profile as the startup selection.
        #[arg(long)]
        persist: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OverrideCommand {
    /// Override a named fan output for a limited time.
    Fan {
        /// Named fan output.
        output: String,
        /// Requested percentage from 0 through 100.
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
        /// Override lifetime, such as `30s`, `10m`, or `1h`.
        #[arg(long = "for", value_parser = parse_duration)]
        duration: Duration,
        /// Permit a duty below the output's configured minimum.
        #[arg(long)]
        allow_below_minimum: bool,
    },
    /// Override a named RGB output for a limited time.
    Led {
        /// Named RGB output.
        output: String,
        /// Red channel from 0 through 255.
        r: u8,
        /// Green channel from 0 through 255.
        g: u8,
        /// Blue channel from 0 through 255.
        b: u8,
        /// Override lifetime, such as `30s`, `10m`, or `1h`.
        #[arg(long = "for", value_parser = parse_duration)]
        duration: Duration,
    },
    /// Clear one override or all active overrides.
    Clear {
        /// Named output whose override should be cleared.
        output: Option<String>,
        /// Clear every active override.
        #[arg(long, conflicts_with = "output")]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OutputCommand {
    /// Restore firmware or driver control for one output or all outputs.
    Release {
        /// Named output. All outputs on its device are released together.
        output: Option<String>,
        /// Release every configured output.
        #[arg(long, conflicts_with = "output")]
        all: bool,
    },
    /// Resume profile control for one output or all outputs.
    Resume {
        /// Named output. All outputs on its device resume together.
        output: Option<String>,
        /// Resume every configured output.
        #[arg(long, conflicts_with = "output")]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Dump read-only Corsair HID endpoint data.
    HidDump {
        /// Configured Corsair device alias.
        device: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match try_main().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            exit_code(&error)
        }
    }
}

async fn try_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    init_tracing(cli.verbose)?;
    let socket = cli.socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    match cli.command {
        Commands::Status { json, watch, check } => handle_status(&socket, json, watch, check).await,
        Commands::Devices { json } => {
            let data = send_request(&socket, Command::Devices).await?;
            let devices: Vec<DeviceDescriptor> = serde_json::from_value(data)
                .context("daemon returned an invalid device payload")?;
            if json {
                print_json(&serde_json::to_value(&devices)?)?;
            } else {
                for device in devices {
                    println!("{}  {}  {}", device.id, device.driver, device.name);
                    for (key, value) in device.details {
                        println!("  {key}: {value}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Config { command } => match command {
            ConfigCommand::Check { path } => {
                Config::from_path(&path)?;
                println!("Config OK: {}", path.display());
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Preview {
                path,
                profile,
                temperatures,
            } => {
                preview_config(&path, profile.as_deref(), temperatures)?;
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Reload => {
                let data = send_request(&socket, Command::ConfigReload).await?;
                let payload: ReloadPayload = decode_payload(data, "configuration reload")?;
                println!(
                    "Reloaded configuration generation {} with profile {}",
                    payload.generation, payload.active_profile
                );
                Ok(ExitCode::SUCCESS)
            }
        },
        Commands::Profile { command } => match command {
            ProfileCommand::List => {
                let data = send_request(&socket, Command::ProfileList).await?;
                let payload: ProfilesPayload = decode_payload(data, "profile list")?;
                for profile in payload.profiles {
                    let active = profile == payload.active_profile;
                    let persisted = payload.persisted_profile.as_deref() == Some(&profile);
                    match (active, persisted) {
                        (true, true) => println!("{profile} (active, startup)"),
                        (true, false) => println!("{profile} (active)"),
                        (false, true) => println!("{profile} (startup)"),
                        (false, false) => println!("{profile}"),
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            ProfileCommand::Set { profile, persist } => {
                let data = send_request(
                    &socket,
                    Command::ProfileSet {
                        profile: profile.clone(),
                        persist,
                    },
                )
                .await?;
                let payload: ActiveProfilePayload = decode_payload(data, "active profile")?;
                if payload.persisted
                    || payload.persisted_profile.as_deref() == Some(&payload.active_profile)
                {
                    println!("Active profile: {} (startup)", payload.active_profile);
                } else {
                    println!("Active profile: {} (runtime only)", payload.active_profile);
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Commands::Override { command } => {
            handle_override(&socket, command).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Output { command } => {
            handle_output(&socket, command).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Debug { command } => match command {
            DebugCommand::HidDump { device } => {
                let data = send_request(&socket, Command::DebugHidDump { device }).await?;
                let payload: HidDumpPayload = decode_payload(data, "HID dump")?;
                tracing::debug!(device = payload.device, "received HID dump");
                print!("{}", payload.dump);
                Ok(ExitCode::SUCCESS)
            }
        },
        Commands::Completions { shell } => {
            let mut command = Cli::command();
            clap_complete::generate(
                Shell::from(shell),
                &mut command,
                "feather",
                &mut io::stdout(),
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn handle_status(
    socket: &Path,
    json: bool,
    watch: Option<Duration>,
    check: bool,
) -> Result<ExitCode> {
    loop {
        let data = send_request(socket, Command::Status).await?;
        let status: DaemonStatus = serde_json::from_value(data.clone())
            .context("daemon returned an invalid status payload")?;
        if json {
            if watch.is_some() {
                println!("{}", serde_json::to_string(&data)?);
            } else {
                print_json(&data)?;
            }
        } else {
            if watch.is_some() && io::stdout().is_terminal() {
                let mut stdout = io::stdout().lock();
                write!(stdout, "\x1b[2J\x1b[H")?;
                stdout.flush()?;
            }
            print_status(&status);
        }

        if check && status.health != Health::Healthy {
            return Ok(ExitCode::from(HEALTH_CHECK_FAILED));
        }
        let Some(interval) = watch else {
            return Ok(ExitCode::SUCCESS);
        };
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            signal = tokio::signal::ctrl_c() => {
                signal.context("could not listen for Ctrl+C")?;
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
}

async fn handle_override(socket: &std::path::Path, command: OverrideCommand) -> Result<()> {
    let command = match command {
        OverrideCommand::Fan {
            output,
            percent,
            duration,
            allow_below_minimum,
        } => Command::OverrideFan {
            output,
            percent,
            duration_ms: duration_ms(duration)?,
            allow_below_minimum,
        },
        OverrideCommand::Led {
            output,
            r,
            g,
            b,
            duration,
        } => Command::OverrideRgb {
            output,
            rgb: [r, g, b],
            duration_ms: duration_ms(duration)?,
        },
        OverrideCommand::Clear { output, all } => {
            require_target(&output, all)?;
            Command::OverrideClear { output }
        }
    };
    let data = send_request(socket, command).await?;
    print_json(&data)
}

async fn handle_output(socket: &std::path::Path, command: OutputCommand) -> Result<()> {
    let command = match command {
        OutputCommand::Release { output, all } => {
            require_target(&output, all)?;
            Command::OutputRelease { output }
        }
        OutputCommand::Resume { output, all } => {
            require_target(&output, all)?;
            Command::OutputResume { output }
        }
    };
    let data = send_request(socket, command).await?;
    print_json(&data)
}

fn require_target(output: &Option<String>, all: bool) -> Result<()> {
    if output.is_none() && !all {
        anyhow::bail!("provide an output name or use --all");
    }
    Ok(())
}

fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".into());
    }
    Ok(duration)
}

fn parse_temperature(value: &str) -> std::result::Result<f64, String> {
    let temperature = value
        .parse::<f64>()
        .map_err(|error| format!("invalid temperature: {error}"))?;
    if temperature.is_finite() {
        Ok(temperature)
    } else {
        Err("temperature must be finite".into())
    }
}

fn duration_ms(duration: Duration) -> Result<u64> {
    duration
        .as_millis()
        .try_into()
        .context("override duration is too large")
}

fn init_tracing(verbose: u8) -> Result<()> {
    let default_level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("could not initialize logging: {error}"))
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn decode_payload<T: DeserializeOwned>(value: serde_json::Value, name: &str) -> Result<T> {
    serde_json::from_value(value)
        .with_context(|| format!("daemon returned an invalid {name} payload"))
}

fn preview_config(
    path: &std::path::Path,
    profile: Option<&str>,
    mut temperatures: Vec<f64>,
) -> Result<()> {
    let config = Config::from_path(path)?;
    let profile_name = profile.unwrap_or(&config.default_profile);
    let profile = config
        .profiles
        .get(profile_name)
        .ok_or_else(|| anyhow::anyhow!("unknown profile '{profile_name}'"))?;
    if temperatures.is_empty() {
        temperatures.extend(
            config
                .curves
                .values()
                .flat_map(|curve| curve.points.iter().map(|point| point.temp)),
        );
        temperatures.extend(
            config
                .color_curves
                .values()
                .flat_map(|curve| curve.points.iter().map(|point| point.temp)),
        );
        temperatures.sort_by(f64::total_cmp);
        temperatures.dedup_by(|left, right| left.total_cmp(right).is_eq());
    }

    println!("Profile: {profile_name}");
    let local_now = Local::now();
    for temperature in temperatures {
        println!("{temperature} C");
        for (output_name, assignment) in &profile.outputs {
            let output = config
                .outputs
                .get(output_name)
                .ok_or_else(|| anyhow::anyhow!("unknown output '{output_name}'"))?;
            match output {
                OutputConfig::Fan {
                    minimum_percent, ..
                } => {
                    let curve_name = assignment.curve.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("output '{output_name}' has no fan curve")
                    })?;
                    let curve = config
                        .curves
                        .get(curve_name)
                        .ok_or_else(|| anyhow::anyhow!("unknown curve '{curve_name}'"))?;
                    let percent = interpolate_fan(temperature, curve).max(*minimum_percent);
                    println!("  {output_name}: {percent}%");
                }
                OutputConfig::Rgb { .. } => {
                    let curve_name = assignment.color_curve.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("output '{output_name}' has no color curve")
                    })?;
                    let curve = config
                        .color_curves
                        .get(curve_name)
                        .ok_or_else(|| anyhow::anyhow!("unknown color curve '{curve_name}'"))?;
                    let scheduled_off = assignment.off_schedule.as_ref().is_some_and(|name| {
                        config.schedules.get(name).is_some_and(|schedule| {
                            schedule_active(
                                local_now.hour(),
                                local_now.minute(),
                                &schedule.start,
                                &schedule.end,
                            )
                        })
                    });
                    let rgb = if scheduled_off {
                        [0, 0, 0]
                    } else {
                        interpolate_color(temperature, curve)
                    };
                    let [red, green, blue] = rgb;
                    println!("  {output_name}: rgb({red},{green},{blue})");
                }
            }
        }
    }
    Ok(())
}

fn print_status(status: &DaemonStatus) {
    println!(
        "featherd {}  profile={}  health={:?}  uptime={}s  generation={}",
        status.version,
        status.active_profile,
        status.health,
        status.uptime_seconds,
        status.config_generation
    );
    println!("Sensors:");
    for (name, sensor) in &status.sensors {
        match &sensor.reading {
            Some(reading) => println!(
                "  {name}: {:.1} C ({:?}, {} ms old)",
                reading.celsius, sensor.health, reading.age_ms
            ),
            None => println!("  {name}: unavailable ({:?})", sensor.health),
        }
        if let Some(error) = &sensor.error {
            println!("    error: {error}");
        }
    }
    println!("Outputs:");
    for (name, output) in &status.outputs {
        println!(
            "  {name}: {:?} {:?} target={} observed={}",
            output.kind,
            output.health,
            output
                .target
                .as_ref()
                .map_or_else(|| "none".into(), ToString::to_string),
            output
                .observed
                .as_ref()
                .map_or_else(|| "none".into(), ToString::to_string)
        );
        if let Some(error) = &output.error {
            println!("    error: {error}");
        }
    }
    if !status.overrides.is_empty() {
        println!("Overrides:");
        for active in &status.overrides {
            println!(
                "  {}: {} ({} ms remaining)",
                active.output, active.value, active.remaining_ms
            );
        }
    }
}

fn exit_code(error: &anyhow::Error) -> ExitCode {
    match error.downcast_ref::<FeatherError>() {
        Some(FeatherError::Config(_) | FeatherError::Protocol(_)) => ExitCode::from(2),
        Some(FeatherError::Daemon(_)) => ExitCode::from(3),
        Some(FeatherError::Driver(_)) => ExitCode::from(4),
        Some(FeatherError::Io(_) | FeatherError::Json(_)) | None => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_rejects_zero() -> anyhow::Result<()> {
        assert!(parse_duration("0s").is_err());
        assert_eq!(
            parse_duration("15s").map_err(anyhow::Error::msg)?.as_secs(),
            15
        );
        Ok(())
    }

    #[test]
    fn all_or_named_target_is_required() {
        assert!(require_target(&None, false).is_err());
        assert!(require_target(&None, true).is_ok());
        assert!(require_target(&Some("case".into()), false).is_ok());
    }

    #[test]
    fn parses_watch_persistence_and_low_duty_confirmation() -> anyhow::Result<()> {
        let cli = Cli::try_parse_from(["feather", "status", "--watch"])?;
        let Commands::Status { watch, check, .. } = cli.command else {
            anyhow::bail!("status command parsed as another command");
        };
        assert_eq!(watch, Some(Duration::from_secs(2)));
        assert!(!check);

        let cli = Cli::try_parse_from(["feather", "profile", "set", "quiet", "--persist"])?;
        let Commands::Profile {
            command: ProfileCommand::Set { profile, persist },
        } = cli.command
        else {
            anyhow::bail!("profile command parsed as another command");
        };
        assert_eq!(profile, "quiet");
        assert!(persist);

        let cli = Cli::try_parse_from([
            "feather",
            "override",
            "fan",
            "case",
            "20",
            "--for",
            "30s",
            "--allow-below-minimum",
        ])?;
        let Commands::Override {
            command:
                OverrideCommand::Fan {
                    allow_below_minimum,
                    ..
                },
        } = cli.command
        else {
            anyhow::bail!("fan override parsed as another command");
        };
        assert!(allow_below_minimum);
        Ok(())
    }

    #[test]
    fn command_surface_has_no_daemon_or_direct_hardware_mode() -> anyhow::Result<()> {
        let command = Cli::command();
        assert!(
            command
                .get_subcommands()
                .all(|subcommand| subcommand.get_name() != "daemon")
        );
        let devices = command
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "devices")
            .ok_or_else(|| anyhow::anyhow!("devices command is missing"))?;
        assert!(
            devices
                .get_arguments()
                .all(|argument| argument.get_id() != "direct")
        );
        Ok(())
    }
}
