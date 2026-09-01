//! `featherd` sensor polling, curve evaluation, fail-safe handling, and live overrides.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use chrono::{DateTime, Local, Timelike};
use serde_json::{Value, json};

use feather_core::{
    config::{AssignmentConfig, Config, OutputConfig},
    curve::{interpolate_color, interpolate_fan, schedule_active},
    error::{FeatherError, Result},
    types::{
        DaemonStatus, DeviceDescriptor, Health, OutputKind, OutputStatus, OverrideKind,
        OverrideStatus, STATUS_SCHEMA_VERSION, SensorReading, SensorStatus,
    },
};

use crate::{
    VERSION,
    drivers::{DisplayTarget, Hardware},
};

#[derive(Clone, Debug)]
enum OverrideValue {
    Fan(u8),
    Rgb([u8; 3]),
    Display(bool),
}

#[derive(Clone, Debug)]
struct ActiveOverride {
    value: OverrideValue,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
struct CachedReading {
    value: f64,
    observed_at: Instant,
}

#[derive(Clone, Debug, Default)]
struct OutputRuntime {
    last_target: Option<Value>,
    last_write: Option<Instant>,
    observed: Option<Value>,
    health: Option<Health>,
    error: Option<String>,
    consecutive_failures: u32,
}

pub(crate) struct Engine {
    config: Config,
    hardware: Box<dyn Hardware>,
    active_profile: String,
    generation: u64,
    started_at: Instant,
    last_sensor_poll: Option<Instant>,
    readings: BTreeMap<String, CachedReading>,
    sensor_errors: BTreeMap<String, String>,
    outputs: BTreeMap<String, OutputRuntime>,
    overrides: BTreeMap<String, ActiveOverride>,
    released: BTreeSet<String>,
    devices: Vec<DeviceDescriptor>,
    driver_errors: BTreeMap<String, String>,
}

impl Engine {
    pub(crate) fn new(
        config: Config,
        mut hardware: Box<dyn Hardware>,
        initial_profile: Option<String>,
    ) -> Result<Self> {
        let devices = hardware.preflight(&config)?;
        hardware.commit_config(&config);
        let active_profile = initial_profile.unwrap_or_else(|| config.default_profile.clone());
        if !config.profiles.contains_key(&active_profile) {
            return Err(FeatherError::Config(format!(
                "initial profile '{active_profile}' does not exist"
            )));
        }
        Ok(Self {
            config,
            hardware,
            active_profile,
            generation: 1,
            started_at: Instant::now(),
            last_sensor_poll: None,
            readings: BTreeMap::new(),
            sensor_errors: BTreeMap::new(),
            outputs: BTreeMap::new(),
            overrides: BTreeMap::new(),
            released: BTreeSet::new(),
            devices,
            driver_errors: BTreeMap::new(),
        })
    }

    pub(crate) fn tick_interval(&self) -> Duration {
        let refresh = self
            .config
            .outputs
            .values()
            .filter_map(|output| match output {
                OutputConfig::Fan {
                    refresh_interval, ..
                } => Some(*refresh_interval),
                OutputConfig::Rgb { .. } => None,
                OutputConfig::Display { .. } => None,
            })
            .min()
            .unwrap_or(self.config.daemon.poll_interval);
        self.config
            .daemon
            .poll_interval
            .min(refresh)
            .min(Duration::from_secs(1))
    }

    pub(crate) fn tick(&mut self, now: Instant, local_now: DateTime<Local>) -> bool {
        self.overrides.retain(|_, active| active.expires_at > now);
        let poll_due = self.last_sensor_poll.is_none_or(|last| {
            now.saturating_duration_since(last) >= self.config.daemon.poll_interval
        });
        if poll_due {
            self.poll_sensors(now);
            self.last_sensor_poll = Some(now);
        }

        let Some(profile) = self.config.profiles.get(&self.active_profile).cloned() else {
            self.driver_errors.insert(
                "policy".into(),
                format!("active profile '{}' is missing", self.active_profile),
            );
            return true;
        };

        for (output_name, assignment) in profile.outputs {
            let previous = self.outputs.get(&output_name).map(|runtime| {
                (
                    runtime.health.clone().unwrap_or(Health::Unavailable),
                    runtime.error.clone(),
                )
            });
            if let Err(error) = self.evaluate_output(&output_name, &assignment, now, local_now) {
                let runtime = self.outputs.entry(output_name.clone()).or_default();
                runtime.health = Some(Health::Degraded);
                runtime.error = Some(error.to_string());
                runtime.consecutive_failures = runtime.consecutive_failures.saturating_add(1);
            }
            let current = self.outputs.get(&output_name).map(|runtime| {
                (
                    runtime.health.clone().unwrap_or(Health::Unavailable),
                    runtime.error.clone(),
                )
            });
            log_output_transition(&output_name, previous.as_ref(), current.as_ref());
        }

        self.outputs.iter().any(|(name, runtime)| {
            runtime.consecutive_failures >= self.config.daemon.failure_limit
                && self.fan_failure_stops_daemon(name)
        })
    }

    fn poll_sensors(&mut self, now: Instant) {
        for (name, config) in &self.config.sensors {
            match self.hardware.read_sensor(name, config) {
                Ok(value) if value.is_finite() && (-50.0..=150.0).contains(&value) => {
                    self.readings.insert(
                        name.clone(),
                        CachedReading {
                            value,
                            observed_at: now,
                        },
                    );
                    update_sensor_error(&mut self.sensor_errors, name, None);
                }
                Ok(value) => {
                    update_sensor_error(
                        &mut self.sensor_errors,
                        name,
                        Some(format!("invalid temperature {value}")),
                    );
                }
                Err(error) => {
                    update_sensor_error(&mut self.sensor_errors, name, Some(error.to_string()));
                }
            }
        }
    }

    fn evaluate_output(
        &mut self,
        output_name: &str,
        assignment: &AssignmentConfig,
        now: Instant,
        local_now: DateTime<Local>,
    ) -> Result<()> {
        let output = self
            .config
            .outputs
            .get(output_name)
            .cloned()
            .ok_or_else(|| FeatherError::Config(format!("unknown output '{output_name}'")))?;
        if self.released.contains(output_name) {
            let runtime = self.outputs.entry(output_name.into()).or_default();
            runtime.health = Some(Health::Released);
            runtime.error = None;
            runtime.consecutive_failures = 0;
            return Ok(());
        }

        let active_override = self.overrides.get(output_name).cloned();
        match (&output, active_override.map(|active| active.value)) {
            (OutputConfig::Fan { .. }, Some(OverrideValue::Fan(percent))) => {
                return self.write_fan(output_name, &output, percent, Health::Healthy, now);
            }
            (OutputConfig::Rgb { .. }, Some(OverrideValue::Rgb(rgb))) => {
                return self.write_rgb(output_name, &output, rgb, Health::Healthy, now);
            }
            (
                OutputConfig::Display {
                    brightness_percent, ..
                },
                Some(OverrideValue::Display(enabled)),
            ) => {
                let target = if enabled {
                    DisplayTarget::Awake {
                        brightness_percent: *brightness_percent,
                    }
                } else {
                    DisplayTarget::Sleep
                };
                return self.write_display(output_name, &output, target, now);
            }
            (_, Some(_)) => {
                return Err(FeatherError::Daemon(format!(
                    "override type does not match output '{output_name}'"
                )));
            }
            (_, None) => {}
        }

        let temperature = assignment
            .sources
            .iter()
            .map(|source| {
                self.readings
                    .get(source)
                    .filter(|reading| {
                        now.saturating_duration_since(reading.observed_at)
                            <= self.config.daemon.stale_after
                    })
                    .map(|reading| reading.value)
                    .ok_or(source)
            })
            .collect::<std::result::Result<Vec<_>, _>>();

        match output {
            OutputConfig::Fan {
                minimum_percent,
                fail_safe_percent,
                hysteresis_percent,
                ..
            } => {
                let (mut target, health) = match temperature {
                    Ok(values) => {
                        let reading = values.into_iter().fold(f64::NEG_INFINITY, f64::max);
                        let curve_name = assignment.curve.as_deref().ok_or_else(|| {
                            FeatherError::Config(format!("output '{output_name}' has no fan curve"))
                        })?;
                        let curve = self.config.curves.get(curve_name).ok_or_else(|| {
                            FeatherError::Config(format!("unknown curve '{curve_name}'"))
                        })?;
                        (
                            interpolate_fan(reading, curve).max(minimum_percent),
                            Health::Healthy,
                        )
                    }
                    Err(_source) => (fail_safe_percent, Health::FailSafe),
                };
                if health == Health::Healthy
                    && let Some(previous) = self
                        .outputs
                        .get(output_name)
                        .and_then(|runtime| runtime.last_target.as_ref())
                        .and_then(Value::as_u64)
                        .and_then(|value| u8::try_from(value).ok())
                    && previous.abs_diff(target) < hysteresis_percent
                {
                    target = previous;
                }
                let output_config =
                    self.config
                        .outputs
                        .get(output_name)
                        .cloned()
                        .ok_or_else(|| {
                            FeatherError::Config(format!("unknown output '{output_name}'"))
                        })?;
                self.write_fan(output_name, &output_config, target, health, now)
            }
            OutputConfig::Rgb { .. } => {
                let sensors_available = temperature.is_ok();
                let rgb = match temperature {
                    Ok(values) => {
                        let reading = values.into_iter().fold(f64::NEG_INFINITY, f64::max);
                        let curve_name = assignment.color_curve.as_deref().ok_or_else(|| {
                            FeatherError::Config(format!(
                                "output '{output_name}' has no color curve"
                            ))
                        })?;
                        let curve = self.config.color_curves.get(curve_name).ok_or_else(|| {
                            FeatherError::Config(format!("unknown color curve '{curve_name}'"))
                        })?;
                        if assignment.off_schedule.as_ref().is_some_and(|name| {
                            self.config.schedules.get(name).is_some_and(|schedule| {
                                schedule_active(
                                    local_now.hour(),
                                    local_now.minute(),
                                    &schedule.start,
                                    &schedule.end,
                                )
                            })
                        }) {
                            [0, 0, 0]
                        } else {
                            interpolate_color(reading, curve)
                        }
                    }
                    Err(_) => [0, 0, 0],
                };
                let health = if sensors_available {
                    Health::Healthy
                } else {
                    Health::FailSafe
                };
                let output_config =
                    self.config
                        .outputs
                        .get(output_name)
                        .cloned()
                        .ok_or_else(|| {
                            FeatherError::Config(format!("unknown output '{output_name}'"))
                        })?;
                self.write_rgb(output_name, &output_config, rgb, health, now)
            }
            OutputConfig::Display {
                brightness_percent, ..
            } => {
                let scheduled_off = assignment.off_schedule.as_ref().is_some_and(|name| {
                    self.config.schedules.get(name).is_some_and(|schedule| {
                        schedule_active(
                            local_now.hour(),
                            local_now.minute(),
                            &schedule.start,
                            &schedule.end,
                        )
                    })
                });
                let target = if scheduled_off {
                    DisplayTarget::Sleep
                } else {
                    DisplayTarget::Awake { brightness_percent }
                };
                let output_config =
                    self.config
                        .outputs
                        .get(output_name)
                        .cloned()
                        .ok_or_else(|| {
                            FeatherError::Config(format!("unknown output '{output_name}'"))
                        })?;
                self.write_display(output_name, &output_config, target, now)
            }
        }
    }

    fn write_fan(
        &mut self,
        output_name: &str,
        config: &OutputConfig,
        percent: u8,
        health: Health,
        now: Instant,
    ) -> Result<()> {
        self.hardware.check_fan_health(config)?;
        let refresh = match config {
            OutputConfig::Fan {
                refresh_interval, ..
            } => *refresh_interval,
            OutputConfig::Rgb { .. } | OutputConfig::Display { .. } => Duration::ZERO,
        };
        let target = json!(percent);
        let needs_periodic_refresh = self.hardware.fan_needs_periodic_refresh(config);
        let needs_write = self.outputs.get(output_name).is_none_or(|runtime| {
            runtime.last_target.as_ref() != Some(&target)
                || runtime.consecutive_failures > 0
                || needs_periodic_refresh
                    && runtime
                        .last_write
                        .is_none_or(|last| now.saturating_duration_since(last) >= refresh)
        });
        if !needs_write {
            let runtime = self.outputs.entry(output_name.into()).or_default();
            runtime.health = Some(fan_health(&health, runtime.error.is_some()));
            runtime.consecutive_failures = 0;
            return Ok(());
        }
        let write = self.hardware.set_fan(output_name, config, percent)?;
        let runtime = self.outputs.entry(output_name.into()).or_default();
        runtime.last_target = Some(target);
        runtime.last_write = Some(now);
        runtime.observed = Some(write.observed);
        runtime.health = Some(fan_health(&health, write.warning.is_some()));
        runtime.error = write.warning;
        runtime.consecutive_failures = 0;
        Ok(())
    }

    fn fan_failure_stops_daemon(&self, output_name: &str) -> bool {
        let Some(config @ OutputConfig::Fan { .. }) = self.config.outputs.get(output_name) else {
            return false;
        };
        !self.hardware.fan_failure_is_isolated(config)
    }

    fn write_rgb(
        &mut self,
        output_name: &str,
        config: &OutputConfig,
        rgb: [u8; 3],
        health: Health,
        now: Instant,
    ) -> Result<()> {
        let target = json!(rgb);
        let needs_write = self
            .outputs
            .get(output_name)
            .is_none_or(|runtime| runtime.last_target.as_ref() != Some(&target));
        if !needs_write {
            let runtime = self.outputs.entry(output_name.into()).or_default();
            runtime.health = Some(health);
            runtime.error = None;
            runtime.consecutive_failures = 0;
            return Ok(());
        }
        let observed = self.hardware.set_rgb(output_name, config, rgb)?;
        let runtime = self.outputs.entry(output_name.into()).or_default();
        runtime.last_target = Some(target);
        runtime.last_write = Some(now);
        runtime.observed = Some(observed);
        runtime.health = Some(health);
        runtime.error = None;
        runtime.consecutive_failures = 0;
        Ok(())
    }

    fn write_display(
        &mut self,
        output_name: &str,
        config: &OutputConfig,
        display_target: DisplayTarget,
        now: Instant,
    ) -> Result<()> {
        let target = match display_target {
            DisplayTarget::Awake { brightness_percent } => json!(brightness_percent),
            DisplayTarget::Sleep => json!("sleep"),
        };
        let needs_write = self
            .outputs
            .get(output_name)
            .is_none_or(|runtime| runtime.last_target.as_ref() != Some(&target));
        if !needs_write {
            let runtime = self.outputs.entry(output_name.into()).or_default();
            runtime.health = Some(Health::Healthy);
            runtime.error = None;
            runtime.consecutive_failures = 0;
            return Ok(());
        }
        let observed = self
            .hardware
            .set_display(output_name, config, display_target)?;
        let runtime = self.outputs.entry(output_name.into()).or_default();
        runtime.last_target = Some(target);
        runtime.last_write = Some(now);
        runtime.observed = Some(observed);
        runtime.health = Some(Health::Healthy);
        runtime.error = None;
        runtime.consecutive_failures = 0;
        Ok(())
    }

    pub(crate) fn status(&self, now: Instant) -> DaemonStatus {
        let sensors: BTreeMap<String, SensorStatus> = self
            .config
            .sensors
            .iter()
            .map(|(name, config)| {
                let reading = self.readings.get(name).map(|reading| SensorReading {
                    celsius: reading.value,
                    age_ms: duration_millis(now.saturating_duration_since(reading.observed_at)),
                });
                let stale = reading.as_ref().is_none_or(|reading| {
                    reading.age_ms > duration_millis(self.config.daemon.stale_after)
                });
                let driver = match config {
                    feather_core::config::SensorConfig::LinuxHwmon { .. } => "linux-hwmon",
                    feather_core::config::SensorConfig::NvidiaNvml { .. } => "nvidia-nvml",
                    feather_core::config::SensorConfig::CorsairIcueLink { .. } => {
                        "corsair-icue-link"
                    }
                };
                (
                    name.clone(),
                    SensorStatus {
                        driver: driver.into(),
                        health: if stale {
                            Health::Unavailable
                        } else if self.sensor_errors.contains_key(name) {
                            Health::Degraded
                        } else {
                            Health::Healthy
                        },
                        reading,
                        error: self.sensor_errors.get(name).cloned(),
                    },
                )
            })
            .collect();
        let outputs = self
            .config
            .outputs
            .iter()
            .map(|(name, config)| {
                let runtime = self.outputs.get(name);
                (
                    name.clone(),
                    OutputStatus {
                        kind: match config {
                            OutputConfig::Fan { .. } => OutputKind::Fan,
                            OutputConfig::Rgb { .. } => OutputKind::Rgb,
                            OutputConfig::Display { .. } => OutputKind::Display,
                        },
                        health: runtime
                            .and_then(|runtime| runtime.health.clone())
                            .unwrap_or(Health::Unavailable),
                        target: runtime.and_then(|runtime| runtime.last_target.clone()),
                        observed: runtime.and_then(|runtime| runtime.observed.clone()),
                        error: runtime.and_then(|runtime| runtime.error.clone()),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let health = if outputs
            .values()
            .any(|output| output.health == Health::FailSafe)
        {
            Health::FailSafe
        } else if outputs
            .values()
            .any(|output| matches!(output.health, Health::Degraded | Health::Unavailable))
            || sensors
                .values()
                .any(|sensor| matches!(sensor.health, Health::Degraded | Health::Unavailable))
        {
            Health::Degraded
        } else {
            Health::Healthy
        };
        DaemonStatus {
            schema_version: STATUS_SCHEMA_VERSION,
            version: VERSION.into(),
            active_profile: self.active_profile.clone(),
            config_generation: self.generation,
            uptime_seconds: now.saturating_duration_since(self.started_at).as_secs(),
            health,
            sensors,
            outputs,
            overrides: self
                .overrides
                .iter()
                .map(|(output, active)| OverrideStatus {
                    output: output.clone(),
                    kind: match active.value {
                        OverrideValue::Fan(_) => OverrideKind::Fan,
                        OverrideValue::Rgb(_) => OverrideKind::Rgb,
                        OverrideValue::Display(_) => OverrideKind::Display,
                    },
                    value: match active.value {
                        OverrideValue::Fan(value) => json!(value),
                        OverrideValue::Rgb(value) => json!(value),
                        OverrideValue::Display(value) => json!(value),
                    },
                    remaining_ms: duration_millis(active.expires_at.saturating_duration_since(now)),
                })
                .collect(),
            driver_errors: self.driver_errors.clone(),
        }
    }

    pub(crate) fn devices(&mut self) -> Result<Vec<DeviceDescriptor>> {
        match self.hardware.discover() {
            Ok(devices) => {
                self.devices = devices.clone();
                Ok(devices)
            }
            Err(error) => {
                if self.devices.is_empty() {
                    Err(error)
                } else {
                    Ok(self.devices.clone())
                }
            }
        }
    }

    pub(crate) fn profiles(&self) -> Vec<String> {
        self.config.profiles.keys().cloned().collect()
    }

    pub(crate) fn active_profile(&self) -> &str {
        &self.active_profile
    }

    pub(crate) fn set_profile(&mut self, profile: &str) -> Result<()> {
        if !self.config.profiles.contains_key(profile) {
            return Err(FeatherError::Daemon(format!("unknown profile '{profile}'")));
        }
        let current_devices = self.controlled_profile_devices();
        let next_devices = self.profile_devices(profile);
        for device in current_devices.difference(&next_devices) {
            self.release_device(device)?;
        }
        self.active_profile = profile.into();
        self.overrides.clear();
        self.released.clear();
        self.outputs.clear();
        Ok(())
    }

    pub(crate) fn set_fan_override(
        &mut self,
        output: &str,
        percent: u8,
        ttl: Duration,
        allow_below_minimum: bool,
    ) -> Result<()> {
        if ttl.is_zero() {
            return Err(FeatherError::Daemon(
                "override duration must be greater than zero".into(),
            ));
        }
        if percent > 100 {
            return Err(FeatherError::Daemon(
                "fan override percentage must be at most 100".into(),
            ));
        }
        let minimum_percent = match self.config.outputs.get(output) {
            Some(OutputConfig::Fan {
                minimum_percent, ..
            }) => *minimum_percent,
            Some(OutputConfig::Rgb { .. }) => {
                return Err(FeatherError::Daemon(format!(
                    "output '{output}' is not a fan"
                )));
            }
            Some(OutputConfig::Display { .. }) => {
                return Err(FeatherError::Daemon(format!(
                    "output '{output}' is not a fan"
                )));
            }
            None => return Err(FeatherError::Daemon(format!("unknown output '{output}'"))),
        };
        if percent < minimum_percent && !allow_below_minimum {
            return Err(FeatherError::Daemon(format!(
                "fan override {percent}% is below output '{output}' minimum {minimum_percent}%; use --allow-below-minimum to confirm"
            )));
        }
        self.require_active_output(output)?;
        let expires_at = Instant::now()
            .checked_add(ttl)
            .ok_or_else(|| FeatherError::Daemon("override duration is too large".into()))?;
        self.overrides.insert(
            output.into(),
            ActiveOverride {
                value: OverrideValue::Fan(percent),
                expires_at,
            },
        );
        Ok(())
    }

    pub(crate) fn set_rgb_override(
        &mut self,
        output: &str,
        rgb: [u8; 3],
        ttl: Duration,
    ) -> Result<()> {
        if ttl.is_zero() {
            return Err(FeatherError::Daemon(
                "override duration must be greater than zero".into(),
            ));
        }
        match self.config.outputs.get(output) {
            Some(OutputConfig::Rgb { .. }) => {}
            Some(OutputConfig::Fan { .. } | OutputConfig::Display { .. }) => {
                return Err(FeatherError::Daemon(format!(
                    "output '{output}' is not RGB"
                )));
            }
            None => return Err(FeatherError::Daemon(format!("unknown output '{output}'"))),
        }
        self.require_active_output(output)?;
        let expires_at = Instant::now()
            .checked_add(ttl)
            .ok_or_else(|| FeatherError::Daemon("override duration is too large".into()))?;
        self.overrides.insert(
            output.into(),
            ActiveOverride {
                value: OverrideValue::Rgb(rgb),
                expires_at,
            },
        );
        Ok(())
    }

    pub(crate) fn set_display_override(
        &mut self,
        output: &str,
        enabled: bool,
        ttl: Duration,
    ) -> Result<()> {
        if ttl.is_zero() {
            return Err(FeatherError::Daemon(
                "override duration must be greater than zero".into(),
            ));
        }
        match self.config.outputs.get(output) {
            Some(OutputConfig::Display { .. }) => {}
            Some(OutputConfig::Fan { .. } | OutputConfig::Rgb { .. }) => {
                return Err(FeatherError::Daemon(format!(
                    "output '{output}' is not a display"
                )));
            }
            None => return Err(FeatherError::Daemon(format!("unknown output '{output}'"))),
        }
        self.require_active_output(output)?;
        let expires_at = Instant::now()
            .checked_add(ttl)
            .ok_or_else(|| FeatherError::Daemon("override duration is too large".into()))?;
        self.overrides.insert(
            output.into(),
            ActiveOverride {
                value: OverrideValue::Display(enabled),
                expires_at,
            },
        );
        Ok(())
    }

    pub(crate) fn clear_overrides(&mut self, output: Option<&str>) -> Result<()> {
        if let Some(output) = output {
            if self.overrides.remove(output).is_none() {
                return Err(FeatherError::Daemon(format!(
                    "output '{output}' has no active override"
                )));
            }
        } else {
            self.overrides.clear();
        }
        Ok(())
    }

    pub(crate) fn release_outputs(&mut self, output: Option<&str>) -> Result<Vec<String>> {
        let devices = self.selected_devices(output)?;
        for device in &devices {
            self.release_device(device)?;
        }
        let targets = self.outputs_for_devices(&devices);
        for name in &targets {
            self.released.insert(name.clone());
            self.overrides.remove(name);
            let runtime = self.outputs.entry(name.clone()).or_default();
            runtime.health = Some(Health::Released);
            runtime.last_target = None;
        }
        Ok(targets)
    }

    pub(crate) fn resume_outputs(&mut self, output: Option<&str>) -> Result<Vec<String>> {
        let devices = self.selected_devices(output)?;
        let targets = self.outputs_for_devices(&devices);
        for name in &targets {
            self.released.remove(name);
            self.outputs.remove(name);
        }
        Ok(targets)
    }

    fn selected_devices(&self, output: Option<&str>) -> Result<BTreeSet<String>> {
        if let Some(output) = output {
            let config = self
                .config
                .outputs
                .get(output)
                .ok_or_else(|| FeatherError::Daemon(format!("unknown output '{output}'")))?;
            Ok(BTreeSet::from([output_device(config).to_owned()]))
        } else {
            Ok(self
                .config
                .outputs
                .values()
                .map(output_device)
                .map(str::to_owned)
                .collect())
        }
    }

    fn outputs_for_devices(&self, devices: &BTreeSet<String>) -> Vec<String> {
        self.config
            .outputs
            .iter()
            .filter(|(_, output)| devices.contains(output_device(output)))
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn profile_devices(&self, profile: &str) -> BTreeSet<String> {
        self.config
            .profiles
            .get(profile)
            .into_iter()
            .flat_map(|profile| profile.outputs.keys())
            .filter_map(|name| self.config.outputs.get(name))
            .map(output_device)
            .map(str::to_owned)
            .collect()
    }

    fn controlled_profile_devices(&self) -> BTreeSet<String> {
        self.config
            .profiles
            .get(&self.active_profile)
            .into_iter()
            .flat_map(|profile| profile.outputs.keys())
            .filter(|name| !self.released.contains(*name))
            .filter_map(|name| self.config.outputs.get(name))
            .map(output_device)
            .map(str::to_owned)
            .collect()
    }

    fn release_device(&mut self, device: &str) -> Result<()> {
        let (name, config) = self
            .config
            .outputs
            .iter()
            .find(|(_, output)| output_device(output) == device)
            .map(|(name, config)| (name.clone(), config.clone()))
            .ok_or_else(|| {
                FeatherError::Daemon(format!("device '{device}' has no configured outputs"))
            })?;
        self.hardware.release(&name, &config)?;
        let outputs = self
            .config
            .outputs
            .iter()
            .filter(|(_, output)| output_device(output) == device)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for output in outputs {
            self.outputs.remove(&output);
            self.overrides.remove(&output);
        }
        Ok(())
    }

    fn require_active_output(&self, output: &str) -> Result<()> {
        let active = self
            .config
            .profiles
            .get(&self.active_profile)
            .is_some_and(|profile| profile.outputs.contains_key(output));
        if active && !self.released.contains(output) {
            Ok(())
        } else {
            Err(FeatherError::Daemon(format!(
                "output '{output}' is not controlled by the active profile"
            )))
        }
    }

    pub(crate) fn reload(&mut self, candidate: Config) -> Result<()> {
        let devices = self.hardware.preflight(&candidate)?;
        let active_profile = if candidate.profiles.contains_key(&self.active_profile) {
            self.active_profile.clone()
        } else {
            candidate.default_profile.clone()
        };
        let controlled_devices = self.controlled_profile_devices();
        for device in controlled_devices {
            self.release_device(&device)?;
        }
        self.hardware.commit_config(&candidate);
        self.config = candidate;
        self.active_profile = active_profile;
        self.generation = self.generation.saturating_add(1);
        self.last_sensor_poll = None;
        self.readings.clear();
        self.sensor_errors.clear();
        self.outputs.clear();
        self.overrides.clear();
        self.released.clear();
        self.devices = devices;
        Ok(())
    }

    pub(crate) fn debug_hid_dump(&mut self, device: &str) -> Result<String> {
        self.hardware.debug_hid_dump(device)
    }

    pub(crate) fn shutdown(&mut self) {
        self.hardware.shutdown();
    }
}

fn update_sensor_error(errors: &mut BTreeMap<String, String>, name: &str, error: Option<String>) {
    let previous = errors.get(name).cloned();
    if previous == error {
        return;
    }
    match error {
        Some(error) => {
            tracing::warn!(sensor = name, %error, "sensor read failed");
            errors.insert(name.to_owned(), error);
        }
        None => {
            if previous.is_some() {
                tracing::info!(sensor = name, "sensor recovered");
            }
            errors.remove(name);
        }
    }
}

fn log_output_transition(
    name: &str,
    previous: Option<&(Health, Option<String>)>,
    current: Option<&(Health, Option<String>)>,
) {
    if previous == current {
        return;
    }
    let Some((health, error)) = current else {
        return;
    };
    match health {
        Health::Healthy => {
            if previous.is_some() {
                tracing::info!(output = name, "output recovered");
            } else {
                tracing::debug!(output = name, "output ready");
            }
        }
        Health::Released => tracing::info!(output = name, "output released"),
        Health::FailSafe => {
            if let Some(error) = error {
                tracing::warn!(output = name, %error, "output entered fail-safe mode");
            } else {
                tracing::warn!(output = name, "output entered fail-safe mode");
            }
        }
        Health::Degraded | Health::Unavailable => {
            if let Some(error) = error {
                tracing::warn!(output = name, ?health, %error, "output health changed");
            } else {
                tracing::warn!(output = name, ?health, "output health changed");
            }
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn fan_health(policy_health: &Health, has_hardware_warning: bool) -> Health {
    if *policy_health == Health::Healthy && has_hardware_warning {
        Health::Degraded
    } else {
        policy_health.clone()
    }
}

fn output_device(output: &OutputConfig) -> &str {
    match output {
        OutputConfig::Fan { device, .. }
        | OutputConfig::Rgb { device, .. }
        | OutputConfig::Display { device, .. } => device,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};

    use super::*;
    use feather_core::{config::SensorConfig, types::DeviceDescriptor};

    const ENGINE_CONFIG: &str = r#"
schema_version = 1
default_profile = "balanced"

[daemon]
poll_interval = "1s"
stale_after = "2s"
failure_limit = 3

[devices.hub]
driver = "corsair-icue-link"
unique_id = "TEST"
interface = 0

[sensors.cpu]
driver = "linux-hwmon"
device = "test-device"
chip = "test-chip"
label = "test-label"

[curves.main]
points = [{ temp = 40, percent = 40 }, { temp = 60, percent = 60 }]

[outputs.case]
kind = "fan"
device = "hub"
channels = [0]
minimum_percent = 30
fail_safe_percent = 100
hysteresis_percent = 2
refresh_interval = "60s"

[profiles.balanced.outputs.case]
sources = ["cpu"]
curve = "main"
"#;

    const GROUPED_OUTPUT_CONFIG: &str = r#"
schema_version = 1
default_profile = "balanced"

[devices.hub]
driver = "corsair-icue-link"
unique_id = "TEST"
interface = 0

[sensors.cpu]
driver = "linux-hwmon"
device = "test-device"
chip = "test-chip"
label = "test-label"

[curves.main]
points = [{ temp = 40, percent = 40 }, { temp = 60, percent = 60 }]

[outputs.case]
kind = "fan"
device = "hub"
channels = [0]
minimum_percent = 30
fail_safe_percent = 100

[outputs.pump]
kind = "fan"
device = "hub"
channels = [1]
minimum_percent = 30
fail_safe_percent = 100

[profiles.balanced.outputs.case]
sources = ["cpu"]
curve = "main"

[profiles.balanced.outputs.pump]
sources = ["cpu"]
curve = "main"
"#;

    const NVIDIA_OUTPUT_CONFIG: &str = r#"
schema_version = 1
default_profile = "balanced"

[daemon]
poll_interval = "1s"
stale_after = "2s"
failure_limit = 3

[devices.gpu0]
driver = "nvidia-nvml"
uuid = "GPU-test"

[sensors.cpu]
driver = "linux-hwmon"
device = "test-device"
chip = "test-chip"
label = "test-label"

[curves.main]
points = [{ temp = 40, percent = 40 }, { temp = 60, percent = 60 }]

[outputs.gpu_fans]
kind = "fan"
device = "gpu0"
channels = []
minimum_percent = 30
fail_safe_percent = 100
refresh_interval = "1s"

[profiles.balanced.outputs.gpu_fans]
sources = ["cpu"]
curve = "main"
"#;

    const DISPLAY_CONFIG: &str = r#"
schema_version = 1
default_profile = "balanced"

[devices.wireview]
driver = "wire-view-pro-ii"
serial = "2090389E4245"

[schedules.night]
start = "22:00"
end = "07:00"

[outputs.gpu_display]
kind = "display"
device = "wireview"
brightness_percent = 75

[profiles.balanced.outputs.gpu_display]
sources = []
off_schedule = "night"
"#;

    #[derive(Debug, Default)]
    struct FakeState {
        sensor: Option<f64>,
        fan_writes: Vec<(String, u8)>,
        display_writes: Vec<(String, DisplayTarget)>,
        display_error: Option<String>,
        fan_warning: Option<String>,
        fan_error: Option<String>,
        fan_health_error: Option<String>,
        fan_failure_isolated: bool,
        releases: Vec<String>,
        preflights: usize,
        commits: usize,
        shutdowns: usize,
    }

    struct FakeHardware {
        state: Arc<Mutex<FakeState>>,
    }

    impl Hardware for FakeHardware {
        fn preflight(&mut self, _config: &Config) -> Result<Vec<DeviceDescriptor>> {
            lock_state(&self.state).preflights += 1;
            Ok(Vec::new())
        }

        fn commit_config(&mut self, _config: &Config) {
            lock_state(&self.state).commits += 1;
        }

        fn discover(&mut self) -> Result<Vec<DeviceDescriptor>> {
            Ok(Vec::new())
        }

        fn read_sensor(&mut self, _alias: &str, _config: &SensorConfig) -> Result<f64> {
            lock_state(&self.state)
                .sensor
                .ok_or_else(|| FeatherError::Driver("test sensor unavailable".into()))
        }

        fn set_fan(
            &mut self,
            alias: &str,
            _config: &OutputConfig,
            percent: u8,
        ) -> Result<crate::drivers::FanWrite> {
            let mut state = lock_state(&self.state);
            if let Some(error) = &state.fan_error {
                return Err(FeatherError::Driver(error.clone()));
            }
            state.fan_writes.push((alias.to_owned(), percent));
            Ok(crate::drivers::FanWrite {
                observed: json!({ "percent": percent }),
                warning: state.fan_warning.clone(),
            })
        }

        fn fan_needs_periodic_refresh(&self, config: &OutputConfig) -> bool {
            !matches!(config, OutputConfig::Fan { device, .. } if device == "gpu0")
        }

        fn check_fan_health(&self, _config: &OutputConfig) -> Result<()> {
            match &lock_state(&self.state).fan_health_error {
                Some(error) => Err(FeatherError::Driver(error.clone())),
                None => Ok(()),
            }
        }

        fn fan_failure_is_isolated(&self, _config: &OutputConfig) -> bool {
            lock_state(&self.state).fan_failure_isolated
        }

        fn set_rgb(&mut self, _alias: &str, _config: &OutputConfig, rgb: [u8; 3]) -> Result<Value> {
            Ok(json!({ "rgb": rgb }))
        }

        fn set_display(
            &mut self,
            alias: &str,
            _config: &OutputConfig,
            target: DisplayTarget,
        ) -> Result<Value> {
            let mut state = lock_state(&self.state);
            if let Some(error) = &state.display_error {
                return Err(FeatherError::Driver(error.clone()));
            }
            state.display_writes.push((alias.to_owned(), target));
            Ok(json!({ "target": format!("{target:?}") }))
        }

        fn release(&mut self, alias: &str, _config: &OutputConfig) -> Result<()> {
            lock_state(&self.state).releases.push(alias.to_owned());
            Ok(())
        }

        fn debug_hid_dump(&mut self, _device_alias: &str) -> Result<String> {
            Ok("test dump".into())
        }

        fn shutdown(&mut self) {
            lock_state(&self.state).shutdowns += 1;
        }
    }

    fn lock_state(state: &Mutex<FakeState>) -> MutexGuard<'_, FakeState> {
        match state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn test_engine(
        text: &str,
        sensor: Option<f64>,
    ) -> anyhow::Result<(Engine, Arc<Mutex<FakeState>>)> {
        let config = Config::from_toml(text)?;
        let state = Arc::new(Mutex::new(FakeState {
            sensor,
            ..FakeState::default()
        }));
        let hardware = FakeHardware {
            state: Arc::clone(&state),
        };
        let engine = Engine::new(config, Box::new(hardware), None)?;
        Ok((engine, state))
    }

    #[test]
    fn uses_fail_safe_when_a_required_sensor_is_unavailable() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(ENGINE_CONFIG, None)?;
        let now = Instant::now();

        assert!(!engine.tick(now, Local::now()));

        assert_eq!(lock_state(&state).fan_writes, vec![("case".into(), 100)]);
        assert_eq!(engine.status(now).health, Health::FailSafe);
        Ok(())
    }

    #[test]
    fn expires_fan_overrides_and_returns_to_the_curve() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(ENGINE_CONFIG, Some(50.0))?;
        engine.set_fan_override("case", 75, Duration::from_millis(100), false)?;
        let now = Instant::now();

        assert!(!engine.tick(now, Local::now()));
        assert!(!engine.tick(now + Duration::from_secs(2), Local::now()));

        assert_eq!(
            lock_state(&state).fan_writes,
            vec![("case".into(), 75), ("case".into(), 50)]
        );
        Ok(())
    }

    #[test]
    fn blanks_and_restores_a_display_across_the_night_schedule() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(DISPLAY_CONFIG, None)?;
        let now = Instant::now();
        let night = Local::now()
            .with_hour(23)
            .and_then(|value| value.with_minute(0))
            .ok_or_else(|| anyhow::anyhow!("could not construct night test time"))?;
        let day = Local::now()
            .with_hour(12)
            .and_then(|value| value.with_minute(0))
            .ok_or_else(|| anyhow::anyhow!("could not construct day test time"))?;

        assert!(!engine.tick(now, night));
        assert_eq!(
            engine.status(now).outputs["gpu_display"].target,
            Some(json!("sleep"))
        );
        assert!(!engine.tick(now + Duration::from_secs(1), day));

        assert_eq!(
            lock_state(&state).display_writes,
            vec![
                ("gpu_display".into(), DisplayTarget::Sleep),
                (
                    "gpu_display".into(),
                    DisplayTarget::Awake {
                        brightness_percent: 75,
                    },
                ),
            ]
        );
        let status = engine.status(now + Duration::from_secs(1));
        assert_eq!(status.outputs["gpu_display"].kind, OutputKind::Display);
        assert_eq!(status.outputs["gpu_display"].target, Some(json!(75)));
        Ok(())
    }

    #[test]
    fn display_override_temporarily_replaces_the_schedule() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(DISPLAY_CONFIG, None)?;
        let now = Instant::now();
        let day = Local::now()
            .with_hour(12)
            .and_then(|value| value.with_minute(0))
            .ok_or_else(|| anyhow::anyhow!("could not construct day test time"))?;
        engine.set_display_override("gpu_display", false, Duration::from_millis(100))?;

        assert!(!engine.tick(now, day));
        assert!(!engine.tick(now + Duration::from_secs(1), day));

        assert_eq!(
            lock_state(&state).display_writes,
            vec![
                ("gpu_display".into(), DisplayTarget::Sleep),
                (
                    "gpu_display".into(),
                    DisplayTarget::Awake {
                        brightness_percent: 75,
                    },
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn display_failures_do_not_stop_fan_control() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(DISPLAY_CONFIG, None)?;
        lock_state(&state).display_error = Some("display unavailable".into());
        let now = Instant::now();

        for seconds in 0..10 {
            assert!(!engine.tick(now + Duration::from_secs(seconds), Local::now()));
        }

        let status = engine.status(now + Duration::from_secs(10));
        assert_eq!(status.health, Health::Degraded);
        assert_eq!(status.outputs["gpu_display"].health, Health::Degraded);
        Ok(())
    }

    #[test]
    fn nvidia_fan_failures_degrade_without_stopping_other_control() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(NVIDIA_OUTPUT_CONFIG, Some(50.0))?;
        {
            let mut state = lock_state(&state);
            state.fan_error = Some("NVIDIA helper is quarantined".into());
            state.fan_failure_isolated = true;
        }
        let now = Instant::now();

        for seconds in 0..10 {
            assert!(!engine.tick(now + Duration::from_secs(seconds), Local::now()));
        }

        let status = engine.status(now + Duration::from_secs(10));
        assert_eq!(status.health, Health::Degraded);
        assert_eq!(status.outputs["gpu_fans"].health, Health::Degraded);
        assert!(
            status.outputs["gpu_fans"]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("quarantined"))
        );
        Ok(())
    }

    #[test]
    fn non_quarantined_nvidia_fan_failures_reach_the_failure_limit() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(NVIDIA_OUTPUT_CONFIG, Some(50.0))?;
        lock_state(&state).fan_error = Some("NVML permission denied".into());
        let now = Instant::now();

        assert!(!engine.tick(now, Local::now()));
        assert!(!engine.tick(now + Duration::from_secs(1), Local::now()));
        assert!(engine.tick(now + Duration::from_secs(2), Local::now()));
        Ok(())
    }

    #[test]
    fn nvidia_fan_targets_are_not_rewritten_when_unchanged() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(NVIDIA_OUTPUT_CONFIG, Some(50.0))?;
        let now = Instant::now();

        for seconds in 0..10 {
            assert!(!engine.tick(now + Duration::from_secs(seconds), Local::now()));
        }

        assert_eq!(lock_state(&state).fan_writes.len(), 1);
        Ok(())
    }

    #[test]
    fn nvidia_quarantine_degrades_an_unchanged_fan_output_without_rewriting() -> anyhow::Result<()>
    {
        let (mut engine, state) = test_engine(NVIDIA_OUTPUT_CONFIG, Some(50.0))?;
        let now = Instant::now();
        assert!(!engine.tick(now, Local::now()));
        lock_state(&state).fan_health_error = Some("NVIDIA helper is quarantined".into());

        assert!(!engine.tick(now + Duration::from_secs(1), Local::now()));

        assert_eq!(lock_state(&state).fan_writes.len(), 1);
        let status = engine.status(now + Duration::from_secs(1));
        assert_eq!(status.outputs["gpu_fans"].health, Health::Degraded);
        assert!(
            status.outputs["gpu_fans"]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("quarantined"))
        );
        Ok(())
    }

    #[test]
    fn nvidia_fan_retries_after_a_transient_error() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(NVIDIA_OUTPUT_CONFIG, Some(50.0))?;
        let now = Instant::now();
        assert!(!engine.tick(now, Local::now()));

        {
            let mut state = lock_state(&state);
            state.sensor = Some(60.0);
            state.fan_error = Some("temporary NVML failure".into());
        }
        assert!(!engine.tick(now + Duration::from_secs(1), Local::now()));

        {
            let mut state = lock_state(&state);
            state.sensor = Some(50.0);
            state.fan_error = None;
        }
        assert!(!engine.tick(now + Duration::from_secs(2), Local::now()));

        assert_eq!(
            lock_state(&state).fan_writes,
            vec![("gpu_fans".into(), 50), ("gpu_fans".into(), 50)]
        );
        let status = engine.status(now + Duration::from_secs(2));
        assert_eq!(status.outputs["gpu_fans"].health, Health::Healthy);
        assert_eq!(status.outputs["gpu_fans"].error, None);
        Ok(())
    }

    #[test]
    fn repeated_corsair_fan_failures_remain_fatal() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(ENGINE_CONFIG, Some(50.0))?;
        lock_state(&state).fan_error = Some("fan controller unavailable".into());
        let now = Instant::now();

        assert!(!engine.tick(now, Local::now()));
        assert!(!engine.tick(now + Duration::from_secs(1), Local::now()));
        assert!(engine.tick(now + Duration::from_secs(2), Local::now()));
        Ok(())
    }

    #[test]
    fn reports_fail_safe_even_when_the_target_does_not_change() -> anyhow::Result<()> {
        let config = ENGINE_CONFIG.replace(
            "{ temp = 60, percent = 60 }",
            "{ temp = 60, percent = 100 }",
        );
        let (mut engine, state) = test_engine(&config, Some(60.0))?;
        let now = Instant::now();
        assert!(!engine.tick(now, Local::now()));
        lock_state(&state).sensor = None;

        assert!(!engine.tick(now + Duration::from_secs(3), Local::now()));

        assert_eq!(lock_state(&state).fan_writes.len(), 1);
        assert_eq!(
            engine.status(now + Duration::from_secs(3)).health,
            Health::FailSafe
        );
        Ok(())
    }

    #[test]
    fn fan_override_requires_confirmation_below_the_configured_minimum() -> anyhow::Result<()> {
        let (mut engine, _state) = test_engine(ENGINE_CONFIG, Some(50.0))?;

        let Err(error) = engine.set_fan_override("case", 20, Duration::from_secs(10), false) else {
            anyhow::bail!("override below the configured minimum was accepted");
        };
        assert!(error.to_string().contains("--allow-below-minimum"));
        engine.set_fan_override("case", 20, Duration::from_secs(10), true)?;
        Ok(())
    }

    #[test]
    fn preserves_a_hardware_warning_between_refresh_writes() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(ENGINE_CONFIG, Some(50.0))?;
        lock_state(&state).fan_warning = Some("expected 13 fans; enumerated 12".into());
        let now = Instant::now();

        assert!(!engine.tick(now, Local::now()));
        assert!(!engine.tick(now + Duration::from_secs(2), Local::now()));

        assert_eq!(lock_state(&state).fan_writes.len(), 1);
        let status = engine.status(now + Duration::from_secs(2));
        assert_eq!(status.health, Health::Degraded);
        assert_eq!(status.outputs["case"].health, Health::Degraded);
        assert_eq!(
            status.outputs["case"].error.as_deref(),
            Some("expected 13 fans; enumerated 12")
        );
        Ok(())
    }

    #[test]
    fn shutdown_restores_hardware_policy() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(ENGINE_CONFIG, Some(50.0))?;

        engine.shutdown();

        assert_eq!(lock_state(&state).shutdowns, 1);
        Ok(())
    }

    #[test]
    fn releases_all_outputs_that_share_a_device() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(GROUPED_OUTPUT_CONFIG, Some(50.0))?;

        let released = engine.release_outputs(Some("case"))?;

        assert_eq!(released, vec!["case".to_owned(), "pump".to_owned()]);
        assert_eq!(lock_state(&state).releases, vec!["case".to_owned()]);
        let status = engine.status(Instant::now());
        assert_eq!(status.outputs["case"].health, Health::Released);
        assert_eq!(status.outputs["pump"].health, Health::Released);
        Ok(())
    }

    #[test]
    fn reload_preflights_then_releases_then_commits() -> anyhow::Result<()> {
        let (mut engine, state) = test_engine(ENGINE_CONFIG, Some(50.0))?;
        let candidate = Config::from_toml(ENGINE_CONFIG)?;

        engine.reload(candidate)?;

        let state = lock_state(&state);
        assert_eq!(state.preflights, 2);
        assert_eq!(state.releases, vec!["case".to_owned()]);
        assert_eq!(state.commits, 2);
        Ok(())
    }

    #[test]
    fn reload_keeps_an_available_profile_and_falls_back_when_it_is_removed() -> anyhow::Result<()> {
        let config = format!(
            "{ENGINE_CONFIG}\n[profiles.quiet.outputs.case]\nsources = [\"cpu\"]\ncurve = \"main\"\n"
        );
        let (mut engine, _state) = test_engine(&config, Some(50.0))?;
        engine.set_profile("quiet")?;

        engine.reload(Config::from_toml(&config)?)?;
        assert_eq!(engine.active_profile(), "quiet");

        engine.reload(Config::from_toml(ENGINE_CONFIG)?)?;
        assert_eq!(engine.active_profile(), "balanced");
        Ok(())
    }
}
