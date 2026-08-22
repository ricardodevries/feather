//! Hardware ownership and policy execution for the `featherd` process.

#![warn(missing_docs)]

mod daemon;
mod drivers;
mod policy;

pub use daemon::run;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

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
