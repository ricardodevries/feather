//! NVIDIA temperature and fan control for `featherd` through the driver-provided NVML library.

use std::collections::{BTreeMap, BTreeSet};

use nvml_wrapper::{Nvml, enum_wrappers::device::TemperatureSensor};
use serde_json::{Value, json};

use feather_core::{
    config::DeviceConfig,
    error::{FeatherError, Result},
    types::DeviceDescriptor,
};

pub(super) struct NvidiaDriver {
    nvml: Option<Nvml>,
    controlled: BTreeSet<String>,
}

impl NvidiaDriver {
    pub(super) fn new() -> Self {
        Self {
            nvml: None,
            controlled: BTreeSet::new(),
        }
    }

    pub(super) fn discover_configured(
        &mut self,
        configured: &BTreeMap<String, DeviceConfig>,
    ) -> Result<Vec<DeviceDescriptor>> {
        if !configured
            .values()
            .any(|config| matches!(config, DeviceConfig::NvidiaNvml { .. }))
        {
            return Ok(Vec::new());
        }
        let all = self.discover_all()?;
        let mut found = Vec::new();
        for (alias, config) in configured {
            let DeviceConfig::NvidiaNvml { uuid } = config else {
                continue;
            };
            if let Some(device) = all.iter().find(|device| device.id == *uuid) {
                let mut device = device.clone();
                device.details.insert("alias".into(), alias.clone());
                found.push(device);
            }
        }
        Ok(found)
    }

    pub(super) fn discover_all(&mut self) -> Result<Vec<DeviceDescriptor>> {
        let nvml = self.nvml()?;
        let count = nvml
            .device_count()
            .map_err(|error| nvidia_error("could not count GPUs", error))?;
        let mut found = Vec::new();
        for index in 0..count {
            let device = nvml
                .device_by_index(index)
                .map_err(|error| nvidia_error(format!("could not open GPU {index}"), error))?;
            let uuid = device
                .uuid()
                .map_err(|error| nvidia_error(format!("could not read GPU {index} UUID"), error))?;
            let name = device
                .name()
                .unwrap_or_else(|_| format!("NVIDIA GPU {index}"));
            let fan_count = device.num_fans().unwrap_or(0);
            let mut details = BTreeMap::new();
            details.insert("index".into(), index.to_string());
            details.insert("uuid".into(), uuid.clone());
            details.insert("fan_count".into(), fan_count.to_string());
            if let Ok((minimum, maximum)) = device.min_max_fan_speed() {
                details.insert("fan_range".into(), format!("{minimum}-{maximum}"));
            }
            if let Ok(temperature) = device.temperature(TemperatureSensor::Gpu) {
                details.insert("temperature_c".into(), temperature.to_string());
            }
            for fan in 0..fan_count {
                if let Ok(speed) = device.fan_speed(fan) {
                    details.insert(format!("fan{fan}_percent"), speed.to_string());
                }
                if let Ok(policy) = device.fan_control_policy(fan) {
                    details.insert(format!("fan{fan}_policy"), format!("{policy:?}"));
                }
            }
            found.push(DeviceDescriptor {
                id: uuid,
                driver: "nvidia-nvml".into(),
                name,
                details,
            });
        }
        Ok(found)
    }

    pub(super) fn read_temperature(&mut self, alias: &str, config: &DeviceConfig) -> Result<f64> {
        let uuid = uuid(config)?;
        let device = self
            .nvml()?
            .device_by_uuid(uuid)
            .map_err(|error| nvidia_error(format!("device '{alias}' is unavailable"), error))?;
        device
            .temperature(TemperatureSensor::Gpu)
            .map(f64::from)
            .map_err(|error| nvidia_error(format!("could not read '{alias}' temperature"), error))
    }

    pub(super) fn set_fans(
        &mut self,
        alias: &str,
        config: &DeviceConfig,
        channels: &[u32],
        percent: u8,
    ) -> Result<Value> {
        let uuid = uuid(config)?.to_owned();
        self.controlled.insert(uuid.clone());
        let nvml = self.nvml()?;
        let mut device = nvml
            .device_by_uuid(uuid.as_str())
            .map_err(|error| nvidia_error(format!("device '{alias}' is unavailable"), error))?;
        let fan_count = device
            .num_fans()
            .map_err(|error| nvidia_error(format!("could not count fans for '{alias}'"), error))?;
        let targets = selected_channels(channels, fan_count, alias)?;
        let (minimum, maximum) = device.min_max_fan_speed().unwrap_or((0, 100));
        let target = u32::from(percent).clamp(minimum, maximum);
        for fan in &targets {
            device.set_fan_speed(*fan, target).map_err(|error| {
                nvidia_error(format!("could not set '{alias}' fan {fan}"), error)
            })?;
        }
        let speeds = targets
            .iter()
            .map(|fan| {
                device
                    .fan_speed(*fan)
                    .map(|speed| (fan.to_string(), speed))
                    .map_err(|error| {
                        nvidia_error(format!("could not verify '{alias}' fan {fan}"), error)
                    })
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        self.controlled.insert(uuid);
        Ok(json!({
            "percent": target,
            "fans": speeds,
            "range": [minimum, maximum]
        }))
    }

    pub(super) fn release(&mut self, alias: &str, config: &DeviceConfig) -> Result<()> {
        let uuid = uuid(config)?.to_owned();
        let nvml = self.nvml()?;
        let mut device = nvml
            .device_by_uuid(uuid.as_str())
            .map_err(|error| nvidia_error(format!("device '{alias}' is unavailable"), error))?;
        let fan_count = device
            .num_fans()
            .map_err(|error| nvidia_error(format!("could not count fans for '{alias}'"), error))?;
        let mut failures = Vec::new();
        for fan in 0..fan_count {
            if let Err(error) = device.set_default_fan_speed(fan) {
                failures.push(format!("fan {fan}: {error}"));
            }
        }
        if failures.is_empty() {
            self.controlled.remove(&uuid);
            Ok(())
        } else {
            Err(FeatherError::Driver(format!(
                "could not restore '{alias}' automatic fan policy: {}",
                failures.join(", ")
            )))
        }
    }

    pub(super) fn shutdown(&mut self) {
        let uuids = self.controlled.iter().cloned().collect::<Vec<_>>();
        for uuid in uuids {
            let config = DeviceConfig::NvidiaNvml { uuid: uuid.clone() };
            if let Err(error) = self.release(&uuid, &config) {
                tracing::error!(%error, gpu_uuid = uuid, "failed to restore NVIDIA fan policy");
            }
        }
        self.nvml = None;
    }

    fn nvml(&mut self) -> Result<&Nvml> {
        if self.nvml.is_none() {
            self.nvml = Some(Nvml::init().map_err(|error| {
                nvidia_error("could not load or initialize libnvidia-ml.so.1", error)
            })?);
        }
        self.nvml.as_ref().ok_or_else(|| {
            FeatherError::Driver("NVML initialization completed without a library handle".into())
        })
    }
}

impl Drop for NvidiaDriver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn uuid(config: &DeviceConfig) -> Result<&str> {
    match config {
        DeviceConfig::NvidiaNvml { uuid } => Ok(uuid),
        DeviceConfig::CorsairIcueLink { .. } | DeviceConfig::WireViewProIi { .. } => Err(
            FeatherError::Driver("configured device is not an NVIDIA GPU".into()),
        ),
    }
}

fn selected_channels(channels: &[u32], fan_count: u32, alias: &str) -> Result<Vec<u32>> {
    if fan_count == 0 {
        return Err(FeatherError::Driver(format!(
            "device '{alias}' reports no controllable fans"
        )));
    }
    let channels = if channels.is_empty() {
        (0..fan_count).collect::<Vec<_>>()
    } else {
        channels.to_vec()
    };
    if let Some(channel) = channels.iter().find(|channel| **channel >= fan_count) {
        return Err(FeatherError::Driver(format!(
            "fan {channel} does not exist on '{alias}'"
        )));
    }
    Ok(channels)
}

fn nvidia_error(context: impl AsRef<str>, error: impl std::fmt::Display) -> FeatherError {
    FeatherError::Driver(format!("{}: {error}", context.as_ref()))
}
