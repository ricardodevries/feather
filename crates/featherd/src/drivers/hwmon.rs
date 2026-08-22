//! Linux `hwmon` discovery and temperature reads for `featherd`.

use std::{collections::BTreeMap, fs, path::PathBuf};

use feather_core::{
    config::SensorConfig,
    error::{FeatherError, Result},
    types::DeviceDescriptor,
};

pub(super) struct HwmonDriver {
    base: PathBuf,
}

impl HwmonDriver {
    pub(super) fn new() -> Self {
        Self {
            base: PathBuf::from("/sys/class/hwmon"),
        }
    }

    #[cfg(test)]
    fn with_base(base: PathBuf) -> Self {
        Self { base }
    }

    pub(super) fn validate_sensors(&self, sensors: &BTreeMap<String, SensorConfig>) -> Result<()> {
        for (alias, sensor) in sensors {
            if matches!(sensor, SensorConfig::LinuxHwmon { .. }) {
                self.resolve(sensor)
                    .map_err(|error| FeatherError::Driver(format!("sensor '{alias}': {error}")))?;
            }
        }
        Ok(())
    }

    pub(super) fn read(&self, sensor: &SensorConfig) -> Result<f64> {
        let input = self.resolve(sensor)?;
        let raw = fs::read_to_string(&input).map_err(|error| {
            FeatherError::Driver(format!("could not read {}: {error}", input.display()))
        })?;
        let millidegrees: f64 = raw.trim().parse().map_err(|error| {
            FeatherError::Driver(format!("invalid value in {}: {error}", input.display()))
        })?;
        Ok(millidegrees / 1000.0)
    }

    pub(super) fn discover(&self) -> Result<Vec<DeviceDescriptor>> {
        let mut found = Vec::new();
        for directory in sorted_directories(&self.base)? {
            let chip = read_trimmed(directory.join("name")).unwrap_or_else(|_| "unknown".into());
            let physical =
                fs::canonicalize(directory.join("device")).unwrap_or_else(|_| directory.clone());
            let device = physical.display().to_string();
            for input in sorted_temp_inputs(&directory)? {
                let stem = input
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("temp_input")
                    .trim_end_matches("_input");
                let label = read_trimmed(directory.join(format!("{stem}_label")))
                    .unwrap_or_else(|_| stem.into());
                let mut details = BTreeMap::new();
                details.insert("chip".into(), chip.clone());
                details.insert("device".into(), device.clone());
                details.insert("label".into(), label.clone());
                details.insert("input".into(), input.display().to_string());
                found.push(DeviceDescriptor {
                    id: format!("hwmon:{device}:{stem}"),
                    driver: "linux-hwmon".into(),
                    name: format!("{chip}/{label}"),
                    details,
                });
            }
        }
        Ok(found)
    }

    fn resolve(&self, sensor: &SensorConfig) -> Result<PathBuf> {
        let SensorConfig::LinuxHwmon {
            device,
            chip,
            label,
        } = sensor
        else {
            return Err(FeatherError::Driver("sensor is not a hwmon source".into()));
        };
        let mut matches = Vec::new();
        for directory in sorted_directories(&self.base)? {
            let current_chip = match read_trimmed(directory.join("name")) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if current_chip != *chip {
                continue;
            }
            let physical =
                fs::canonicalize(directory.join("device")).unwrap_or_else(|_| directory.clone());
            let physical_text = physical.display().to_string();
            if physical_text != *device
                && !physical_text.ends_with(device)
                && !physical_text.contains(&format!("/{device}/"))
            {
                continue;
            }
            for input in sorted_temp_inputs(&directory)? {
                let stem = input
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .trim_end_matches("_input");
                let current_label = read_trimmed(directory.join(format!("{stem}_label")))
                    .unwrap_or_else(|_| stem.into());
                if current_label == *label {
                    matches.push(input);
                }
            }
        }
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(FeatherError::Driver(format!(
                "hwmon sensor {chip}/{label} on '{device}' was not found"
            ))),
            count => Err(FeatherError::Driver(format!(
                "hwmon sensor {chip}/{label} on '{device}' is ambiguous ({count} matches)"
            ))),
        }
    }
}

fn sorted_directories(base: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut directories = fs::read_dir(base)
        .map_err(|error| {
            FeatherError::Driver(format!("could not scan {}: {error}", base.display()))
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    Ok(directories)
}

fn sorted_temp_inputs(directory: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut inputs = fs::read_dir(directory)
        .map_err(|error| {
            FeatherError::Driver(format!("could not scan {}: {error}", directory.display()))
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("temp") && name.ends_with("_input"))
        })
        .collect::<Vec<_>>();
    inputs.sort();
    Ok(inputs)
}

fn read_trimmed(path: PathBuf) -> std::io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn reads_selected_sensor_by_chip_device_and_label() -> anyhow::Result<()> {
        let root = tempdir()?;
        let physical = root.path().join("devices/0000:00:18.3");
        let hwmon = root.path().join("hwmon/hwmon1");
        fs::create_dir_all(&physical)?;
        fs::create_dir_all(&hwmon)?;
        symlink(&physical, hwmon.join("device"))?;
        fs::write(hwmon.join("name"), "k10temp\n")?;
        fs::write(hwmon.join("temp1_label"), "Tctl\n")?;
        fs::write(hwmon.join("temp1_input"), "67500\n")?;

        let driver = HwmonDriver::with_base(root.path().join("hwmon"));
        let sensor = SensorConfig::LinuxHwmon {
            device: "0000:00:18.3".into(),
            chip: "k10temp".into(),
            label: "Tctl".into(),
        };
        assert_eq!(driver.read(&sensor)?, 67.5);
        let discovered = driver.discover()?;
        let first = discovered
            .first()
            .ok_or_else(|| anyhow::anyhow!("no hwmon sensor was discovered"))?;
        assert_eq!(first.name, "k10temp/Tctl");
        Ok(())
    }
}
