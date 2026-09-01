//! Hardware abstraction and platform-specific driver routing for `featherd`.

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use serde_json::Value;

#[cfg(target_os = "linux")]
use feather_core::config::DeviceConfig;
use feather_core::{
    config::{Config, OutputConfig, SensorConfig},
    error::{FeatherError, Result},
    types::DeviceDescriptor,
};

#[derive(Debug)]
pub(crate) struct FanWrite {
    pub(crate) observed: Value,
    pub(crate) warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayTarget {
    Awake { brightness_percent: u8 },
    Sleep,
}

#[cfg(target_os = "linux")]
impl FanWrite {
    fn healthy(observed: Value) -> Self {
        Self {
            observed,
            warning: None,
        }
    }
}

#[cfg(target_os = "linux")]
mod corsair;
#[cfg(target_os = "linux")]
mod hwmon;
#[cfg(target_os = "linux")]
pub(crate) mod nvidia;
#[cfg(target_os = "linux")]
mod wireview;

/// Hardware operations used by the policy engine.
pub trait Hardware: Send + 'static {
    /// Supplies a progress counter used to prove bounded driver waits remain alive.
    fn set_heartbeat(&mut self, _heartbeat: Arc<AtomicU64>) {}

    /// Validates configured selectors and returns the selected devices.
    ///
    /// # Errors
    ///
    /// Returns an error when a selector is ambiguous, a configured device is
    /// missing, or a driver cannot initialize.
    fn preflight(&mut self, config: &Config) -> Result<Vec<DeviceDescriptor>>;

    /// Activates a configuration that already passed [`Hardware::preflight`].
    fn commit_config(&mut self, config: &Config);

    /// Discovers supported devices without applying a configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no driver can complete discovery.
    fn discover(&mut self) -> Result<Vec<DeviceDescriptor>>;

    /// Reads one configured temperature sensor in degrees Celsius.
    ///
    /// # Errors
    ///
    /// Returns an error when the sensor cannot be resolved or read.
    fn read_sensor(&mut self, alias: &str, config: &SensorConfig) -> Result<f64>;

    /// Sets the duty for a configured fan output and returns observed data.
    ///
    /// # Errors
    ///
    /// Returns an error when the output is not a fan or its driver rejects the write.
    fn set_fan(&mut self, alias: &str, config: &OutputConfig, percent: u8) -> Result<FanWrite>;

    /// Whether an unchanged fan target must be periodically written again.
    ///
    /// Stateful controllers may require refreshes. NVIDIA's manual fan policy
    /// retains its target, so avoiding unchanged writes materially reduces
    /// synchronous NVML traffic.
    fn fan_needs_periodic_refresh(&self, _config: &OutputConfig) -> bool {
        true
    }

    /// Reports a known driver failure before an otherwise unnecessary fan write.
    ///
    /// This is a local health check and must not perform hardware I/O.
    fn check_fan_health(&self, _config: &OutputConfig) -> Result<()> {
        Ok(())
    }

    /// Sets a configured RGB output and returns observed data.
    ///
    /// # Errors
    ///
    /// Returns an error when the output is not RGB or its driver rejects the write.
    fn set_rgb(&mut self, alias: &str, config: &OutputConfig, rgb: [u8; 3]) -> Result<Value>;

    /// Sets a configured display state and returns observed data.
    ///
    /// # Errors
    ///
    /// Returns an error when the output is not a display or its driver rejects the write.
    fn set_display(
        &mut self,
        alias: &str,
        config: &OutputConfig,
        target: DisplayTarget,
    ) -> Result<Value>;

    /// Restores the hardware policy for the physical device owning an output.
    ///
    /// # Errors
    ///
    /// Returns an error when the output cannot be resolved or restored.
    fn release(&mut self, alias: &str, config: &OutputConfig) -> Result<()>;

    /// Reads diagnostic HID data from a configured device.
    ///
    /// # Errors
    ///
    /// Returns an error when the device is not a supported HID device or cannot be read.
    fn debug_hid_dump(&mut self, device_alias: &str) -> Result<String>;

    /// Restores every controlled device before the hardware worker exits.
    fn shutdown(&mut self);
}

#[cfg(target_os = "linux")]
/// Linux hardware router for Corsair HID, hwmon, NVIDIA NVML, and WireView drivers.
pub struct SystemHardware {
    devices: BTreeMap<String, DeviceConfig>,
    corsair: corsair::CorsairDriver,
    hwmon: hwmon::HwmonDriver,
    nvidia: nvidia::NvidiaDriver,
    wireview: wireview::WireViewDriver,
}

#[cfg(target_os = "linux")]
impl SystemHardware {
    /// Creates a hardware router with no selected devices.
    pub fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
            corsair: corsair::CorsairDriver::new(),
            hwmon: hwmon::HwmonDriver::new(),
            nvidia: nvidia::NvidiaDriver::new(),
            wireview: wireview::WireViewDriver::new(),
        }
    }

    fn device(&self, alias: &str) -> Result<&DeviceConfig> {
        self.devices.get(alias).ok_or_else(|| {
            FeatherError::Driver(format!("device alias '{alias}' is not configured"))
        })
    }
}

#[cfg(target_os = "linux")]
impl Default for SystemHardware {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl Hardware for SystemHardware {
    fn set_heartbeat(&mut self, heartbeat: Arc<AtomicU64>) {
        self.nvidia.set_heartbeat(heartbeat);
    }

    fn preflight(&mut self, config: &Config) -> Result<Vec<DeviceDescriptor>> {
        let mut discovered = Vec::new();
        let mut driver_errors = Vec::new();

        match self.corsair.discover_configured(&config.devices) {
            Ok(mut devices) => discovered.append(&mut devices),
            Err(error) => driver_errors.push(error.to_string()),
        }
        match self
            .nvidia
            .discover_configured(&config.devices, config.daemon.driver_timeout)
        {
            Ok(mut devices) => discovered.append(&mut devices),
            Err(error) => driver_errors.push(error.to_string()),
        }
        match self.wireview.discover_configured(&config.devices) {
            Ok(mut devices) => discovered.append(&mut devices),
            Err(error) => driver_errors.push(error.to_string()),
        }
        self.hwmon.validate_sensors(&config.sensors)?;

        let mut claimed = BTreeMap::new();
        for descriptor in &discovered {
            let alias = descriptor
                .details
                .get("alias")
                .map_or("unknown", String::as_str);
            if let Some(previous) = claimed.insert(descriptor.id.as_str(), alias) {
                driver_errors.push(format!(
                    "device aliases '{previous}' and '{alias}' select the same hardware"
                ));
            }
        }

        for (alias, device) in &config.devices {
            let found = discovered
                .iter()
                .any(|descriptor| descriptor.details.get("alias") == Some(alias));
            if !found {
                driver_errors.push(format!("configured device '{alias}' was not found"));
            }
            match device {
                DeviceConfig::CorsairIcueLink { .. }
                | DeviceConfig::NvidiaNvml { .. }
                | DeviceConfig::WireViewProIi { .. } => {}
            }
        }
        if !driver_errors.is_empty() {
            return Err(FeatherError::Driver(driver_errors.join("; ")));
        }

        Ok(discovered)
    }

    fn commit_config(&mut self, config: &Config) {
        self.nvidia.retain_configured(&config.devices);
        self.nvidia.set_timeout(config.daemon.driver_timeout);
        self.devices.clone_from(&config.devices);
    }

    fn discover(&mut self) -> Result<Vec<DeviceDescriptor>> {
        let mut devices = Vec::new();
        let mut errors = Vec::new();
        match self.corsair.discover_all() {
            Ok(mut corsair) => devices.append(&mut corsair),
            Err(error) => errors.push(error),
        }
        match self.nvidia.discover_all() {
            Ok(mut nvidia) => devices.append(&mut nvidia),
            Err(error) => errors.push(error),
        }
        match self.hwmon.discover() {
            Ok(hwmon) => devices.extend(hwmon),
            Err(error) => errors.push(error),
        }
        match self.wireview.discover_all() {
            Ok(mut wireviews) => devices.append(&mut wireviews),
            Err(error) => errors.push(error),
        }
        if devices.is_empty() && !errors.is_empty() {
            return Err(FeatherError::Driver(
                errors
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        for error in errors {
            tracing::debug!(%error, "hardware discovery source unavailable");
        }
        Ok(devices)
    }

    fn read_sensor(&mut self, alias: &str, config: &SensorConfig) -> Result<f64> {
        match config {
            SensorConfig::LinuxHwmon { .. } => self.hwmon.read(config),
            SensorConfig::NvidiaNvml { device } => {
                let device_config = self.device(device)?.clone();
                self.nvidia.read_temperature(alias, &device_config)
            }
            SensorConfig::CorsairIcueLink { device, channel } => {
                let device_config = self.device(device)?.clone();
                self.corsair
                    .read_temperature(device, &device_config, *channel)
            }
        }
    }

    fn set_fan(&mut self, alias: &str, config: &OutputConfig, percent: u8) -> Result<FanWrite> {
        let (device_alias, channels) = match config {
            OutputConfig::Fan {
                device, channels, ..
            } => (device.as_str(), channels.as_slice()),
            OutputConfig::Rgb { .. } | OutputConfig::Display { .. } => {
                return Err(FeatherError::Driver(format!(
                    "output '{alias}' is not a fan"
                )));
            }
        };
        let device_config = self.device(device_alias)?.clone();
        match &device_config {
            DeviceConfig::CorsairIcueLink {
                expected_fan_count, ..
            } => self.corsair.set_fans(
                device_alias,
                &device_config,
                channels,
                percent,
                *expected_fan_count,
            ),
            DeviceConfig::NvidiaNvml { .. } => self
                .nvidia
                .set_fans(device_alias, &device_config, channels, percent)
                .map(FanWrite::healthy),
            DeviceConfig::WireViewProIi { .. } => Err(FeatherError::Driver(
                "WireView devices do not expose system fan control".into(),
            )),
        }
    }

    fn fan_needs_periodic_refresh(&self, config: &OutputConfig) -> bool {
        let OutputConfig::Fan { device, .. } = config else {
            return false;
        };
        !matches!(
            self.devices.get(device),
            Some(DeviceConfig::NvidiaNvml { .. })
        )
    }

    fn check_fan_health(&self, config: &OutputConfig) -> Result<()> {
        let OutputConfig::Fan { device, .. } = config else {
            return Ok(());
        };
        let Some(device_config) = self.devices.get(device) else {
            return Ok(());
        };
        match device_config {
            DeviceConfig::NvidiaNvml { .. } => self.nvidia.check_health(device_config),
            DeviceConfig::CorsairIcueLink { .. } | DeviceConfig::WireViewProIi { .. } => Ok(()),
        }
    }

    fn set_rgb(&mut self, alias: &str, config: &OutputConfig, rgb: [u8; 3]) -> Result<Value> {
        let (device_alias, led_count) = match config {
            OutputConfig::Rgb { device, led_count } => (device.as_str(), *led_count),
            OutputConfig::Fan { .. } | OutputConfig::Display { .. } => {
                return Err(FeatherError::Driver(format!("output '{alias}' is not RGB")));
            }
        };
        let device_config = self.device(device_alias)?.clone();
        match &device_config {
            DeviceConfig::CorsairIcueLink { .. } => {
                self.corsair
                    .set_rgb(device_alias, &device_config, led_count, rgb)
            }
            DeviceConfig::NvidiaNvml { .. } => Err(FeatherError::Driver(
                "NVIDIA devices do not expose RGB control".into(),
            )),
            DeviceConfig::WireViewProIi { .. } => Err(FeatherError::Driver(
                "WireView devices do not expose RGB control".into(),
            )),
        }
    }

    fn set_display(
        &mut self,
        alias: &str,
        config: &OutputConfig,
        target: DisplayTarget,
    ) -> Result<Value> {
        let device_alias = match config {
            OutputConfig::Display { device, .. } => device.as_str(),
            OutputConfig::Fan { .. } | OutputConfig::Rgb { .. } => {
                return Err(FeatherError::Driver(format!(
                    "output '{alias}' is not a display"
                )));
            }
        };
        let device_config = self.device(device_alias)?.clone();
        match &device_config {
            DeviceConfig::WireViewProIi { .. } => {
                self.wireview
                    .set_display(device_alias, &device_config, target)
            }
            DeviceConfig::CorsairIcueLink { .. } | DeviceConfig::NvidiaNvml { .. } => Err(
                FeatherError::Driver(format!("device '{device_alias}' has no display output")),
            ),
        }
    }

    fn release(&mut self, alias: &str, config: &OutputConfig) -> Result<()> {
        let device_alias = match config {
            OutputConfig::Fan { device, .. }
            | OutputConfig::Rgb { device, .. }
            | OutputConfig::Display { device, .. } => device,
        };
        let device_config = self.device(device_alias)?.clone();
        match &device_config {
            DeviceConfig::CorsairIcueLink { .. } => {
                self.corsair.release(device_alias, &device_config)
            }
            DeviceConfig::NvidiaNvml { .. } => self.nvidia.release(alias, &device_config),
            DeviceConfig::WireViewProIi { .. } => {
                let OutputConfig::Display {
                    brightness_percent, ..
                } = config
                else {
                    return Err(FeatherError::Driver(format!(
                        "device '{device_alias}' has an incompatible output"
                    )));
                };
                self.wireview
                    .set_display(
                        alias,
                        &device_config,
                        DisplayTarget::Awake {
                            brightness_percent: *brightness_percent,
                        },
                    )
                    .map(|_| ())
            }
        }
    }

    fn debug_hid_dump(&mut self, device_alias: &str) -> Result<String> {
        let device_config = self.device(device_alias)?.clone();
        match &device_config {
            DeviceConfig::CorsairIcueLink { .. } => {
                self.corsair.debug_dump(device_alias, &device_config)
            }
            DeviceConfig::NvidiaNvml { .. } => Err(FeatherError::Driver(format!(
                "device '{device_alias}' is not a HID device"
            ))),
            DeviceConfig::WireViewProIi { .. } => Err(FeatherError::Driver(format!(
                "device '{device_alias}' is not a HID device"
            ))),
        }
    }

    fn shutdown(&mut self) {
        self.nvidia.shutdown();
        self.corsair.shutdown();
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
/// Stub hardware router used for configuration and client work on non-Linux hosts.
pub struct SystemHardware;

#[cfg(not(target_os = "linux"))]
impl SystemHardware {
    /// Creates the non-Linux hardware stub.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(target_os = "linux"))]
impl Hardware for SystemHardware {
    fn preflight(&mut self, _config: &Config) -> Result<Vec<DeviceDescriptor>> {
        Err(FeatherError::Driver(
            "hardware control is supported only on Linux".into(),
        ))
    }

    fn commit_config(&mut self, _config: &Config) {}

    fn discover(&mut self) -> Result<Vec<DeviceDescriptor>> {
        Err(FeatherError::Driver(
            "hardware discovery is supported only on Linux".into(),
        ))
    }

    fn read_sensor(&mut self, _alias: &str, _config: &SensorConfig) -> Result<f64> {
        Err(FeatherError::Driver("Linux host required".into()))
    }

    fn set_fan(&mut self, _alias: &str, _config: &OutputConfig, _percent: u8) -> Result<FanWrite> {
        Err(FeatherError::Driver("Linux host required".into()))
    }

    fn set_rgb(&mut self, _alias: &str, _config: &OutputConfig, _rgb: [u8; 3]) -> Result<Value> {
        Err(FeatherError::Driver("Linux host required".into()))
    }

    fn set_display(
        &mut self,
        _alias: &str,
        _config: &OutputConfig,
        _target: DisplayTarget,
    ) -> Result<Value> {
        Err(FeatherError::Driver("Linux host required".into()))
    }

    fn release(&mut self, _alias: &str, _config: &OutputConfig) -> Result<()> {
        Ok(())
    }

    fn debug_hid_dump(&mut self, _device_alias: &str) -> Result<String> {
        Err(FeatherError::Driver("Linux host required".into()))
    }

    fn shutdown(&mut self) {}
}
