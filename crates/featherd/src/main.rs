use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use feather_core::error::FeatherError;
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "/etc/feather/config.toml";

#[derive(Debug, Parser)]
#[command(
    name = "featherd",
    version,
    about = "Run the Feather hardware and thermal-control daemon"
)]
struct Args {
    /// TOML configuration file.
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,

    /// Override the Unix socket configured in the TOML file.
    #[arg(long, env = "FEATHER_SOCKET")]
    socket: Option<PathBuf>,

    /// Discover supported hardware without starting the daemon or changing outputs.
    #[arg(long)]
    discover: bool,

    /// Emit discovery results as JSON.
    #[arg(long, requires = "discover")]
    json: bool,

    /// Increase log detail. Repeat for protocol-level logs.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> ExitCode {
    match try_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            exit_code(&error)
        }
    }
}

async fn try_main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.verbose)?;
    if args.discover {
        let devices = featherd::discover_devices()?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&devices)?);
        } else {
            for device in devices {
                println!("{}  {}  {}", device.id, device.driver, device.name);
                for (key, value) in device.details {
                    println!("  {key}: {value}");
                }
            }
        }
        return Ok(());
    }
    featherd::run(args.config, args.socket)
        .await
        .context("featherd stopped with an error")
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

fn exit_code(error: &anyhow::Error) -> ExitCode {
    match error.downcast_ref::<FeatherError>() {
        Some(FeatherError::Config(_) | FeatherError::Protocol(_)) => ExitCode::from(2),
        Some(FeatherError::Daemon(_)) => ExitCode::from(3),
        Some(FeatherError::Driver(_)) => ExitCode::from(4),
        Some(FeatherError::Io(_) | FeatherError::Json(_)) | None => ExitCode::FAILURE,
    }
}
