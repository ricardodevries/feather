//! Hardware ownership and policy execution for the `featherd` process.

#![warn(missing_docs)]

mod daemon;
mod drivers;
mod policy;

pub use daemon::run;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runs the private NVIDIA helper protocol used by a supervising `featherd` process.
///
/// This entry point is public only so the `featherd` binary can dispatch its
/// hidden helper mode. It is not a stable external protocol.
///
/// # Errors
///
/// Returns an error when the helper cannot read a request or write a response.
#[cfg(target_os = "linux")]
pub fn run_nvidia_helper() -> feather_core::error::Result<()> {
    drivers::nvidia::run_helper()
}

/// Discovers supported hardware without starting the daemon or changing outputs.
///
/// # Errors
///
/// Returns an error when no hardware driver can complete discovery.
pub fn discover_devices() -> feather_core::error::Result<Vec<feather_core::types::DeviceDescriptor>>
{
    use drivers::Hardware;

    let mut hardware = drivers::SystemHardware::new();
    hardware.discover()
}
