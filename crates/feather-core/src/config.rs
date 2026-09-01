//! Versioned Feather TOML configuration and reference validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::error::{FeatherError, Result};

/// Configuration schema accepted by this release.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Complete daemon configuration loaded from TOML.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Version of the configuration schema.
    pub schema_version: u32,
    /// Fallback profile used when no saved or current profile is available.
    pub default_profile: String,
    /// Polling, socket, and failure settings.
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Physical devices keyed by operator-defined aliases.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
    /// Temperature sources keyed by aliases.
    #[serde(default)]
    pub sensors: BTreeMap<String, SensorConfig>,
    /// Fan curves keyed by names.
    #[serde(default)]
    pub curves: BTreeMap<String, FanCurve>,
    /// RGB curves keyed by names.
    #[serde(default)]
    pub color_curves: BTreeMap<String, ColorCurve>,
    /// Local-time schedules keyed by names.
    #[serde(default)]
    pub schedules: BTreeMap<String, ScheduleConfig>,
    /// Controllable fan, RGB, and display outputs keyed by names.
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputConfig>,
    /// Policy profiles keyed by names.
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

/// Daemon timing, socket, and failure settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Interval between temperature reads.
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Maximum age of a reading before its output enters fail-safe mode.
    #[serde(with = "humantime_serde")]
    pub stale_after: Duration,
    /// Unix socket used by local clients.
    pub socket: String,
    /// Runtime state persisted across daemon restarts.
    pub state_file: String,
    /// Consecutive output failures that stop the daemon.
    pub failure_limit: u32,
    /// Maximum time allowed for an isolated hardware-driver request.
    #[serde(with = "humantime_serde")]
    pub driver_timeout: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(3),
            stale_after: Duration::from_secs(10),
            socket: "/run/feather/feather.sock".into(),
            state_file: "/var/lib/feather/state.json".into(),
            failure_limit: 5,
            driver_timeout: Duration::from_secs(5),
        }
    }
}

/// Stable selector for a physical output device.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "driver", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DeviceConfig {
    /// Corsair iCUE LINK System Hub selected through HID metadata.
    CorsairIcueLink {
        /// USB vendor ID.
        #[serde(default = "default_corsair_vendor")]
        vendor_id: u16,
        /// USB product ID.
        #[serde(default = "default_corsair_product")]
        product_id: u16,
        /// Optional HID serial used to distinguish multiple hubs.
        unique_id: Option<String>,
        /// HID interface number.
        #[serde(default)]
        interface: i32,
        /// Expected directly connected fan subdevices.
        ///
        /// A mismatch marks fan outputs on this hub as degraded while control
        /// continues for the channels the hub enumerated.
        expected_fan_count: Option<u8>,
    },
    /// NVIDIA GPU selected by its persistent NVML UUID.
    NvidiaNvml {
        /// UUID returned by NVML, including the `GPU-` prefix.
        uuid: String,
    },
    /// Thermal Grizzly WireView Pro II selected by its USB serial number.
    WireViewProIi {
        /// Stable USB serial shown by udev and `feather devices`.
        serial: String,
    },
}

const fn default_corsair_vendor() -> u16 {
    0x1b1c
}

const fn default_corsair_product() -> u16 {
    0x0c3f
}

/// Driver and selector for a temperature source.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "driver", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SensorConfig {
    /// Linux hwmon temperature input.
    LinuxHwmon {
        /// Physical device path or stable path suffix.
        device: String,
        /// hwmon chip name.
        chip: String,
        /// hwmon temperature label.
        label: String,
    },
    /// NVIDIA core temperature from NVML.
    NvidiaNvml {
        /// Alias of an NVIDIA device.
        device: String,
    },
    /// Temperature channel reported by a Corsair hub.
    CorsairIcueLink {
        /// Alias of a Corsair device.
        device: String,
        /// Zero-based hub temperature channel.
        channel: u8,
    },
}

/// Piecewise-linear fan curve.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FanCurve {
    /// Points sorted from low to high temperature.
    pub points: Vec<FanPoint>,
}

/// Temperature and fan-percentage pair.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FanPoint {
    /// Temperature in degrees Celsius.
    pub temp: f64,
    /// Fan target from 0 through 100.
    pub percent: u8,
}

/// Piecewise-linear RGB curve.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColorCurve {
    /// Points sorted from low to high temperature.
    pub points: Vec<ColorPoint>,
}

/// Temperature and RGB color pair.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ColorPoint {
    /// Temperature in degrees Celsius.
    pub temp: f64,
    /// Red, green, and blue channel values.
    pub rgb: [u8; 3],
}

/// Half-open local-time window used to turn RGB and display outputs off.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// Start time in `HH:MM` form.
    pub start: String,
    /// End time in `HH:MM` form.
    pub end: String,
}

/// Physical output controlled by a policy assignment.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum OutputConfig {
    /// One or more fan channels on a configured device.
    Fan {
        /// Device alias.
        device: String,
        /// Fan indices. An empty list selects all eligible fans.
        #[serde(default)]
        channels: Vec<u32>,
        /// Floor applied after curve interpolation.
        #[serde(default)]
        minimum_percent: u8,
        /// Target used when a required sensor is stale or unavailable.
        fail_safe_percent: u8,
        /// Minimum target change before a new value is accepted.
        #[serde(default = "default_hysteresis")]
        hysteresis_percent: u8,
        /// Maximum delay before the current target is written again when the
        /// hardware driver requires periodic refreshes.
        #[serde(default = "default_refresh", with = "humantime_serde")]
        refresh_interval: Duration,
    },
    /// RGB strip attached to a Corsair hub.
    Rgb {
        /// Device alias.
        device: String,
        /// Number of LEDs written by each update.
        led_count: u16,
    },
    /// Backlight on a Thermal Grizzly WireView Pro II display.
    Display {
        /// Device alias.
        device: String,
        /// Backlight percentage used outside the off schedule.
        brightness_percent: u8,
    },
}

const fn default_hysteresis() -> u8 {
    2
}

fn default_refresh() -> Duration {
    Duration::from_secs(2)
}

/// Output assignments for one named operating profile.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Assignments keyed by configured output names.
    #[serde(default)]
    pub outputs: BTreeMap<String, AssignmentConfig>,
}

/// Connects an output to required sensors and policy data.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentConfig {
    /// Required sensor aliases.
    pub sources: Vec<String>,
    /// Method used to combine source temperatures.
    #[serde(default)]
    pub aggregate: Aggregation,
    /// Fan curve name for a fan output.
    pub curve: Option<String>,
    /// Color curve name for an RGB output.
    pub color_curve: Option<String>,
    /// Optional off-schedule name for an RGB or display output.
    pub off_schedule: Option<String>,
}

/// Method used to combine multiple sensor readings.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Aggregation {
    /// Use the highest source temperature.
    #[default]
    Max,
}

impl Config {
    /// Reads, parses, and validates a TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, TOML parsing fails, or
    /// the resulting configuration violates a validation rule.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|error| {
            FeatherError::Config(format!("could not read {}: {error}", path.display()))
        })?;
        Self::from_toml(&text).map_err(|error| match error {
            FeatherError::Config(message) => {
                FeatherError::Config(format!("{}: {message}", path.display()))
            }
            other => other,
        })
    }

    /// Parses and validates a TOML string.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed TOML or an invalid configuration.
    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)
            .map_err(|error| FeatherError::Config(format!("invalid TOML: {error}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks schema versions, value ranges, references, and hardware ownership.
    ///
    /// # Errors
    ///
    /// Returns the first validation error found.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(config_error(format!(
                "schema_version must be {CONFIG_SCHEMA_VERSION}"
            )));
        }
        if self.daemon.poll_interval.is_zero() {
            return Err(config_error(
                "daemon.poll_interval must be greater than zero",
            ));
        }
        if self.daemon.stale_after < self.daemon.poll_interval {
            return Err(config_error(
                "daemon.stale_after must be at least daemon.poll_interval",
            ));
        }
        if self.daemon.socket.is_empty() {
            return Err(config_error("daemon.socket must not be empty"));
        }
        if self.daemon.state_file.is_empty() {
            return Err(config_error("daemon.state_file must not be empty"));
        }
        if self.daemon.failure_limit == 0 {
            return Err(config_error(
                "daemon.failure_limit must be greater than zero",
            ));
        }
        if self.daemon.driver_timeout.is_zero() {
            return Err(config_error(
                "daemon.driver_timeout must be greater than zero",
            ));
        }
        if !self.profiles.contains_key(&self.default_profile) {
            return Err(config_error(format!(
                "default_profile '{}' does not exist",
                self.default_profile
            )));
        }

        let mut selectors = BTreeSet::new();
        for (name, device) in &self.devices {
            let selector = match device {
                DeviceConfig::CorsairIcueLink {
                    vendor_id,
                    product_id,
                    unique_id,
                    interface,
                    expected_fan_count,
                } => {
                    if unique_id
                        .as_ref()
                        .is_some_and(|value| value.trim().is_empty())
                    {
                        return Err(config_error(format!(
                            "devices.{name}.unique_id must not be empty"
                        )));
                    }
                    if let Some(expected_fan_count) = expected_fan_count
                        && !(1..=24).contains(expected_fan_count)
                    {
                        return Err(config_error(format!(
                            "devices.{name}.expected_fan_count must be from 1 through 24"
                        )));
                    }
                    format!(
                        "corsair:{vendor_id:04x}:{product_id:04x}:{}:{interface}",
                        unique_id.as_deref().unwrap_or("*")
                    )
                }
                DeviceConfig::NvidiaNvml { uuid } => {
                    if uuid.trim().is_empty() {
                        return Err(config_error(format!(
                            "devices.{name}.uuid must not be empty"
                        )));
                    }
                    format!("nvidia:{uuid}")
                }
                DeviceConfig::WireViewProIi { serial } => {
                    if serial.trim().is_empty() {
                        return Err(config_error(format!(
                            "devices.{name}.serial must not be empty"
                        )));
                    }
                    format!("wireview:{}", serial.to_ascii_uppercase())
                }
            };
            if !selectors.insert(selector) {
                return Err(config_error(format!(
                    "devices.{name} selects hardware already claimed by another alias"
                )));
            }
        }

        for (name, sensor) in &self.sensors {
            match sensor {
                SensorConfig::LinuxHwmon {
                    device,
                    chip,
                    label,
                } => {
                    if device.is_empty() || chip.is_empty() || label.is_empty() {
                        return Err(config_error(format!(
                            "sensors.{name} requires non-empty device, chip, and label"
                        )));
                    }
                }
                SensorConfig::NvidiaNvml { device } => {
                    require_key("device", name, device, &self.devices)?;
                    if !matches!(
                        self.devices.get(device),
                        Some(DeviceConfig::NvidiaNvml { .. })
                    ) {
                        return Err(config_error(format!(
                            "sensors.{name} requires an nvidia-nvml device"
                        )));
                    }
                }
                SensorConfig::CorsairIcueLink { device, .. } => {
                    require_key("device", name, device, &self.devices)?;
                    if !matches!(
                        self.devices.get(device),
                        Some(DeviceConfig::CorsairIcueLink { .. })
                    ) {
                        return Err(config_error(format!(
                            "sensors.{name} requires a corsair-icue-link device"
                        )));
                    }
                }
            }
        }

        for (name, curve) in &self.curves {
            validate_points(
                &format!("curves.{name}"),
                curve.points.iter().map(|point| point.temp),
            )?;
            if let Some(point) = curve.points.iter().find(|point| point.percent > 100) {
                return Err(config_error(format!(
                    "curves.{name} contains invalid fan percentage {}",
                    point.percent
                )));
            }
        }
        for (name, curve) in &self.color_curves {
            validate_points(
                &format!("color_curves.{name}"),
                curve.points.iter().map(|point| point.temp),
            )?;
        }
        for (name, schedule) in &self.schedules {
            parse_time(&schedule.start)
                .map_err(|message| config_error(format!("schedules.{name}.start {message}")))?;
            parse_time(&schedule.end)
                .map_err(|message| config_error(format!("schedules.{name}.end {message}")))?;
        }

        for (name, output) in &self.outputs {
            let device = match output {
                OutputConfig::Fan {
                    device,
                    channels,
                    minimum_percent,
                    fail_safe_percent,
                    hysteresis_percent,
                    refresh_interval,
                    ..
                } => {
                    for (field, value) in [
                        ("minimum_percent", *minimum_percent),
                        ("fail_safe_percent", *fail_safe_percent),
                        ("hysteresis_percent", *hysteresis_percent),
                    ] {
                        if value > 100 {
                            return Err(config_error(format!(
                                "outputs.{name}.{field} must be at most 100"
                            )));
                        }
                    }
                    if fail_safe_percent < minimum_percent {
                        return Err(config_error(format!(
                            "outputs.{name}.fail_safe_percent must be at least minimum_percent"
                        )));
                    }
                    if refresh_interval.is_zero() {
                        return Err(config_error(format!(
                            "outputs.{name}.refresh_interval must be greater than zero"
                        )));
                    }
                    let mut unique_channels = BTreeSet::new();
                    if let Some(channel) = channels
                        .iter()
                        .find(|channel| !unique_channels.insert(**channel))
                    {
                        return Err(config_error(format!(
                            "outputs.{name}.channels contains duplicate channel {channel}"
                        )));
                    }
                    device
                }
                OutputConfig::Rgb { device, led_count } => {
                    if *led_count == 0 {
                        return Err(config_error(format!(
                            "outputs.{name}.led_count must be greater than zero"
                        )));
                    }
                    if !matches!(
                        self.devices.get(device),
                        Some(DeviceConfig::CorsairIcueLink { .. })
                    ) {
                        return Err(config_error(format!(
                            "outputs.{name} requires a corsair-icue-link device"
                        )));
                    }
                    device
                }
                OutputConfig::Display {
                    device,
                    brightness_percent,
                } => {
                    if !(1..=100).contains(brightness_percent) {
                        return Err(config_error(format!(
                            "outputs.{name}.brightness_percent must be from 1 through 100"
                        )));
                    }
                    if !matches!(
                        self.devices.get(device),
                        Some(DeviceConfig::WireViewProIi { .. })
                    ) {
                        return Err(config_error(format!(
                            "outputs.{name} requires a wire-view-pro-ii device"
                        )));
                    }
                    device
                }
            };
            require_key("device", name, device, &self.devices)?;
            if matches!(output, OutputConfig::Fan { .. })
                && !matches!(
                    self.devices.get(device),
                    Some(DeviceConfig::CorsairIcueLink { .. } | DeviceConfig::NvidiaNvml { .. })
                )
            {
                return Err(config_error(format!(
                    "outputs.{name} requires a fan-capable device"
                )));
            }
            if let OutputConfig::Fan { channels, .. } = output
                && matches!(
                    self.devices.get(device),
                    Some(DeviceConfig::CorsairIcueLink { .. })
                )
                && let Some(channel) = channels
                    .iter()
                    .find(|channel| **channel > u32::from(u8::MAX))
            {
                return Err(config_error(format!(
                    "outputs.{name}.channels contains Corsair channel {channel}, which is over 255"
                )));
            }
        }

        for device_name in self.devices.keys() {
            let mut all_fans = None;
            let mut fan_channels = BTreeMap::new();
            let mut rgb_output = None;
            let mut display_output = None;
            for (output_name, output) in &self.outputs {
                if output_device(output) != device_name {
                    continue;
                }
                match output {
                    OutputConfig::Fan { channels, .. } if channels.is_empty() => {
                        if let Some(previous) =
                            all_fans.or_else(|| fan_channels.values().next().copied())
                        {
                            return Err(config_error(format!(
                                "outputs.{output_name} and outputs.{previous} both claim fans on device '{device_name}'"
                            )));
                        }
                        all_fans = Some(output_name.as_str());
                    }
                    OutputConfig::Fan { channels, .. } => {
                        if let Some(previous) = all_fans {
                            return Err(config_error(format!(
                                "outputs.{output_name} and outputs.{previous} both claim fans on device '{device_name}'"
                            )));
                        }
                        for channel in channels {
                            if let Some(previous) =
                                fan_channels.insert(*channel, output_name.as_str())
                            {
                                return Err(config_error(format!(
                                    "outputs.{output_name} and outputs.{previous} both claim fan channel {channel} on device '{device_name}'"
                                )));
                            }
                        }
                    }
                    OutputConfig::Rgb { .. } => {
                        if let Some(previous) = rgb_output.replace(output_name.as_str()) {
                            return Err(config_error(format!(
                                "outputs.{output_name} and outputs.{previous} both claim RGB on device '{device_name}'"
                            )));
                        }
                    }
                    OutputConfig::Display { .. } => {
                        if let Some(previous) = display_output.replace(output_name.as_str()) {
                            return Err(config_error(format!(
                                "outputs.{output_name} and outputs.{previous} both claim the display on device '{device_name}'"
                            )));
                        }
                    }
                }
            }
        }

        for (profile_name, profile) in &self.profiles {
            if profile.outputs.is_empty() {
                return Err(config_error(format!(
                    "profiles.{profile_name}.outputs must not be empty"
                )));
            }
            for (output_name, assignment) in &profile.outputs {
                let output = self.outputs.get(output_name).ok_or_else(|| {
                    config_error(format!(
                        "profiles.{profile_name}.outputs.{output_name} references an unknown output"
                    ))
                })?;
                for source in &assignment.sources {
                    if !self.sensors.contains_key(source) {
                        return Err(config_error(format!(
                            "profiles.{profile_name}.outputs.{output_name} references unknown sensor '{source}'"
                        )));
                    }
                }
                match output {
                    OutputConfig::Fan { .. } => {
                        if assignment.sources.is_empty() {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name}.sources must not be empty"
                            )));
                        }
                        let curve = assignment.curve.as_ref().ok_or_else(|| {
                            config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} requires curve"
                            ))
                        })?;
                        if !self.curves.contains_key(curve) {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} references unknown curve '{curve}'"
                            )));
                        }
                        if assignment.color_curve.is_some() || assignment.off_schedule.is_some() {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} is a fan and cannot use color_curve or off_schedule"
                            )));
                        }
                    }
                    OutputConfig::Rgb { .. } => {
                        if assignment.sources.is_empty() {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name}.sources must not be empty"
                            )));
                        }
                        let curve = assignment.color_curve.as_ref().ok_or_else(|| {
                            config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} requires color_curve"
                            ))
                        })?;
                        if !self.color_curves.contains_key(curve) {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} references unknown color curve '{curve}'"
                            )));
                        }
                        if assignment.curve.is_some() {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} is RGB and cannot use curve"
                            )));
                        }
                        if let Some(schedule) = &assignment.off_schedule
                            && !self.schedules.contains_key(schedule)
                        {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} references unknown schedule '{schedule}'"
                            )));
                        }
                    }
                    OutputConfig::Display { .. } => {
                        if !assignment.sources.is_empty()
                            || assignment.curve.is_some()
                            || assignment.color_curve.is_some()
                        {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} is a display and accepts only off_schedule"
                            )));
                        }
                        if let Some(schedule) = &assignment.off_schedule
                            && !self.schedules.contains_key(schedule)
                        {
                            return Err(config_error(format!(
                                "profiles.{profile_name}.outputs.{output_name} references unknown schedule '{schedule}'"
                            )));
                        }
                    }
                }
            }

            for device_name in self.devices.keys() {
                let mut configured = 0usize;
                let mut assigned = 0usize;
                let mut missing = None;
                for (output_name, output) in &self.outputs {
                    if output_device(output) == device_name {
                        configured += 1;
                        if profile.outputs.contains_key(output_name) {
                            assigned += 1;
                        } else if missing.is_none() {
                            missing = Some(output_name);
                        }
                    }
                }
                if assigned > 0 && assigned < configured {
                    return Err(config_error(format!(
                        "profiles.{profile_name} assigns only part of device '{device_name}'; output '{}' is missing",
                        missing.map_or("unknown", String::as_str)
                    )));
                }
            }
        }
        Ok(())
    }
}

fn output_device(output: &OutputConfig) -> &str {
    match output {
        OutputConfig::Fan { device, .. }
        | OutputConfig::Rgb { device, .. }
        | OutputConfig::Display { device, .. } => device,
    }
}

fn require_key<T>(kind: &str, owner: &str, key: &str, map: &BTreeMap<String, T>) -> Result<()> {
    if map.contains_key(key) {
        Ok(())
    } else {
        Err(config_error(format!(
            "{owner} references unknown {kind} '{key}'"
        )))
    }
}

fn validate_points(name: &str, points: impl Iterator<Item = f64>) -> Result<()> {
    let mut count = 0;
    let mut previous = None;
    for temperature in points {
        if !temperature.is_finite() {
            return Err(config_error(format!("{name} temperatures must be finite")));
        }
        if let Some(previous) = previous
            && temperature <= previous
        {
            return Err(config_error(format!(
                "{name} temperatures must increase from low to high"
            )));
        }
        previous = Some(temperature);
        count += 1;
    }
    if count == 0 {
        return Err(config_error(format!("{name}.points must not be empty")));
    }
    Ok(())
}

pub(crate) fn parse_time(value: &str) -> std::result::Result<(u8, u8), String> {
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| "must use HH:MM format".to_owned())?;
    if hour.len() != 2 || minute.len() != 2 {
        return Err("must use HH:MM format".into());
    }
    let hour: u8 = hour
        .parse()
        .map_err(|_| "must use HH:MM format".to_owned())?;
    let minute: u8 = minute
        .parse()
        .map_err(|_| "must use HH:MM format".to_owned())?;
    if hour > 23 || minute > 59 {
        return Err("must be a valid 24-hour time".into());
    }
    Ok((hour, minute))
}

fn config_error(message: impl Into<String>) -> FeatherError {
    FeatherError::Config(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema_version = 1
default_profile = "balanced"

[devices.hub]
driver = "corsair-icue-link"
unique_id = "ABC"
interface = 0
expected_fan_count = 13

[sensors.cpu]
driver = "linux-hwmon"
device = "0000:00:18.3"
chip = "k10temp"
label = "Tctl"

[curves.case]
points = [{ temp = 40, percent = 30 }, { temp = 80, percent = 100 }]

[outputs.case]
kind = "fan"
device = "hub"
channels = []
minimum_percent = 30
fail_safe_percent = 100

[profiles.balanced.outputs.case]
sources = ["cpu"]
curve = "case"
"#;

    const DISPLAY: &str = r#"
schema_version = 1
default_profile = "balanced"

[devices.gpu_display]
driver = "wire-view-pro-ii"
serial = "2090389E4245"

[schedules.night]
start = "22:00"
end = "07:00"

[outputs.gpu_display]
kind = "display"
device = "gpu_display"
brightness_percent = 75

[profiles.balanced.outputs.gpu_display]
sources = []
off_schedule = "night"
"#;

    #[test]
    fn accepts_valid_config() -> anyhow::Result<()> {
        let config = Config::from_toml(VALID)?;
        assert_eq!(config.default_profile, "balanced");
        assert!(matches!(
            config.devices.get("hub"),
            Some(DeviceConfig::CorsairIcueLink {
                expected_fan_count: Some(13),
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn accepts_a_scheduled_wireview_display() -> anyhow::Result<()> {
        let config = Config::from_toml(DISPLAY)?;
        assert!(matches!(
            config.devices.get("gpu_display"),
            Some(DeviceConfig::WireViewProIi { serial }) if serial == "2090389E4245"
        ));
        assert!(matches!(
            config.outputs.get("gpu_display"),
            Some(OutputConfig::Display {
                brightness_percent: 75,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn rejects_a_display_with_temperature_sources() -> anyhow::Result<()> {
        let invalid = DISPLAY.replace("sources = []", "sources = [\"missing\"]");
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("display temperature source was accepted");
        };
        assert!(error.to_string().contains("unknown sensor 'missing'"));
        Ok(())
    }

    #[test]
    fn rejects_display_brightness_over_one_hundred() -> anyhow::Result<()> {
        let invalid = DISPLAY.replace("brightness_percent = 75", "brightness_percent = 101");
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("display brightness over 100 was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("brightness_percent must be from 1 through 100")
        );
        Ok(())
    }

    #[test]
    fn rejects_an_impossible_expected_corsair_fan_count() -> anyhow::Result<()> {
        let invalid = VALID.replace("expected_fan_count = 13", "expected_fan_count = 25");
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("expected Corsair fan count over 24 was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("expected_fan_count must be from 1 through 24")
        );
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields() -> anyhow::Result<()> {
        let result = Config::from_toml(&format!("{VALID}\nno_such_field = true"));
        let Err(error) = result else {
            anyhow::bail!("configuration with an unknown field was accepted");
        };
        let error = error.to_string();
        assert!(error.contains("unknown field"), "{error}");
        Ok(())
    }

    #[test]
    fn rejects_a_zero_driver_timeout() -> anyhow::Result<()> {
        let invalid = VALID.replace(
            "default_profile = \"balanced\"",
            "default_profile = \"balanced\"\n\n[daemon]\ndriver_timeout = \"0s\"",
        );
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("zero driver timeout was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("daemon.driver_timeout must be greater than zero")
        );
        Ok(())
    }

    #[test]
    fn rejects_missing_profile_reference() -> anyhow::Result<()> {
        let invalid = VALID.replace("curve = \"case\"", "curve = \"missing\"");
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("configuration with a missing curve was accepted");
        };
        let error = error.to_string();
        assert!(error.contains("unknown curve 'missing'"), "{error}");
        Ok(())
    }

    #[test]
    fn parses_wrapping_schedule_times() -> anyhow::Result<()> {
        assert_eq!(parse_time("22:00").map_err(anyhow::Error::msg)?, (22, 0));
        assert!(parse_time("24:00").is_err());
        assert!(parse_time("7:00").is_err());
        Ok(())
    }

    #[test]
    fn rejects_curve_percentages_over_one_hundred() -> anyhow::Result<()> {
        let invalid = VALID.replace(
            "{ temp = 80, percent = 100 }",
            "{ temp = 80, percent = 101 }",
        );
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("curve percentage over 100 was accepted");
        };
        assert!(error.to_string().contains("invalid fan percentage 101"));
        Ok(())
    }

    #[test]
    fn rejects_a_sensor_attached_to_the_wrong_driver() -> anyhow::Result<()> {
        let invalid = VALID.replace(
            "driver = \"linux-hwmon\"\ndevice = \"0000:00:18.3\"\nchip = \"k10temp\"\nlabel = \"Tctl\"",
            "driver = \"nvidia-nvml\"\ndevice = \"hub\"",
        );
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("NVIDIA sensor attached to a Corsair device was accepted");
        };
        assert!(error.to_string().contains("requires an nvidia-nvml device"));
        Ok(())
    }

    #[test]
    fn rejects_partial_device_ownership_in_a_profile() -> anyhow::Result<()> {
        let invalid = format!(
            "{VALID}\n[color_curves.light]\npoints = [{{ temp = 40, rgb = [0, 0, 0] }}]\n\
             [outputs.light]\nkind = \"rgb\"\ndevice = \"hub\"\nled_count = 1\n"
        );
        let Err(error) = Config::from_toml(&invalid) else {
            anyhow::bail!("partial device ownership was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("assigns only part of device 'hub'")
        );
        Ok(())
    }
}
