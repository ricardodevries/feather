//! Versioned status and discovery types shared by the daemon and client.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current local IPC protocol version.
pub const IPC_PROTOCOL_VERSION: u32 = 3;
/// Current JSON status document version.
pub const STATUS_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Health state reported for the daemon, a sensor, or an output.
pub enum Health {
    /// The component is operating without a known error.
    Healthy,
    /// The component is operating with a recoverable error or cached data.
    Degraded,
    /// The output uses its configured fail-safe value.
    FailSafe,
    /// `featherd` has returned the device to its hardware policy.
    Released,
    /// No valid reading or output state is available.
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// A device found by a hardware driver.
pub struct DeviceDescriptor {
    /// Stable hardware identifier, such as a GPU UUID or HID unique ID.
    pub id: String,
    /// Driver name used to access the device.
    pub driver: String,
    /// Human-readable device name.
    pub name: String,
    /// Driver-specific discovery fields.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// One temperature reading and its age.
pub struct SensorReading {
    /// Temperature in degrees Celsius.
    pub celsius: f64,
    /// Milliseconds since the successful read.
    pub age_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// Runtime state for a configured sensor.
pub struct SensorStatus {
    /// Driver that owns the sensor.
    pub driver: String,
    /// Current sensor health.
    pub health: Health,
    /// Latest usable reading, when available.
    pub reading: Option<SensorReading>,
    /// Most recent read error, when present.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
/// Kind of physical output controlled by a policy.
pub enum OutputKind {
    /// Fan duty output.
    Fan,
    /// RGB LED output.
    Rgb,
    /// Display power-state and brightness output.
    Display,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// Runtime state for a configured output.
pub struct OutputStatus {
    /// Fan, RGB, or display output type.
    pub kind: OutputKind,
    /// Current output health.
    pub health: Health,
    /// Value requested by the policy engine.
    pub target: Option<serde_json::Value>,
    /// Value reported by the driver after a write.
    pub observed: Option<serde_json::Value>,
    /// Most recent write error, when present.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
/// Kind of temporary output override.
pub enum OverrideKind {
    /// Fan duty override.
    Fan,
    /// RGB color override.
    Rgb,
    /// Display on/off override.
    Display,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// Active temporary override reported by the daemon.
pub struct OverrideStatus {
    /// Output alias receiving the override.
    pub output: String,
    /// Fan or RGB override type.
    pub kind: OverrideKind,
    /// Requested fan percentage, RGB value, or display state.
    pub value: serde_json::Value,
    /// Milliseconds until the override expires.
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// Versioned daemon status returned by the `status` command.
pub struct DaemonStatus {
    /// [`STATUS_SCHEMA_VERSION`] used to encode this document.
    pub schema_version: u32,
    /// Running `featherd` version.
    pub version: String,
    /// Profile currently driving output assignments.
    pub active_profile: String,
    /// Number of successfully activated configurations.
    pub config_generation: u64,
    /// Seconds since the daemon started.
    pub uptime_seconds: u64,
    /// Aggregate daemon health.
    pub health: Health,
    /// Sensor states keyed by configuration alias.
    pub sensors: BTreeMap<String, SensorStatus>,
    /// Output states keyed by configuration alias.
    pub outputs: BTreeMap<String, OutputStatus>,
    /// Temporary overrides that have not expired.
    pub overrides: Vec<OverrideStatus>,
    /// Latest driver errors keyed by sensor or output alias.
    pub driver_errors: BTreeMap<String, String>,
}
