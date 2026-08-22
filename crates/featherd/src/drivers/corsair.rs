//! Corsair iCUE LINK System Hub protocol over HID for `featherd`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    thread,
    time::Duration,
};

use hidapi::{HidApi, HidDevice};
use serde_json::{Value, json};

use feather_core::{
    config::DeviceConfig,
    error::{FeatherError, Result},
    types::DeviceDescriptor,
};

use super::FanWrite;

const BUFFER_SIZE: usize = 512;
const WRITE_SIZE: usize = BUFFER_SIZE + 1;
const HEADER: usize = 3;
const WRITE_HEADER: usize = 4;

const CMD_SOFTWARE_MODE: &[u8] = &[0x01, 0x03, 0x00, 0x02];
const CMD_HARDWARE_MODE: &[u8] = &[0x01, 0x03, 0x00, 0x01];
const CMD_OPEN: &[u8] = &[0x0d, 0x01];
const CMD_CLOSE: &[u8] = &[0x05, 0x01, 0x01];
const CMD_READ: &[u8] = &[0x08, 0x01];
const CMD_WRITE: &[u8] = &[0x06, 0x01];

const EP_DEVICES: u8 = 0x36;
const EP_SPEEDS: u8 = 0x17;
const EP_TEMPERATURES: u8 = 0x21;
const EP_SET_SPEED: u8 = 0x18;

const DT_DEVICES: &[u8] = &[0x21, 0x00];
const DT_SPEEDS: &[u8] = &[0x25, 0x00];
const DT_TEMPERATURES: &[u8] = &[0x10, 0x00];
const DT_SET_SPEED: &[u8] = &[0x07, 0x00];

const CMD_OPEN_COLOR: &[u8] = &[0x0d, 0x00];
const CMD_CLOSE_COLOR: &[u8] = &[0x05, 0x01];
const CMD_WRITE_COLOR: &[u8] = &[0x06, 0x00];
const EP_SET_COLOR: u8 = 0x22;
const DT_SET_COLOR: &[u8] = &[0x12, 0x00];

#[derive(Clone, Debug, PartialEq, Eq)]
struct LinkDevice {
    channel: u8,
    model: u8,
    variant: u8,
    id: String,
}

#[derive(Debug, PartialEq, Eq)]
struct LinkTopology {
    last_channel: u8,
    devices: Vec<LinkDevice>,
    consumed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinkDeviceProfile {
    name: &'static str,
    controls_speed: bool,
    is_fan: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpeedReading {
    channel: u8,
    status: u8,
    rpm: Option<i16>,
}

pub(super) struct CorsairDriver {
    hubs: BTreeMap<String, Hub>,
}

impl CorsairDriver {
    pub(super) fn new() -> Self {
        Self {
            hubs: BTreeMap::new(),
        }
    }

    pub(super) fn discover_configured(
        &self,
        configured: &BTreeMap<String, DeviceConfig>,
    ) -> Result<Vec<DeviceDescriptor>> {
        if !configured
            .values()
            .any(|config| matches!(config, DeviceConfig::CorsairIcueLink { .. }))
        {
            return Ok(Vec::new());
        }
        let api = HidApi::new().map_err(hid_error("could not initialize HIDAPI"))?;
        let mut found = Vec::new();
        for (alias, config) in configured {
            let DeviceConfig::CorsairIcueLink { .. } = config else {
                continue;
            };
            let mut matches = api
                .device_list()
                .filter(|info| matches_device(info, config));
            let Some(info) = matches.next() else {
                continue;
            };
            if matches.next().is_some() {
                return Err(FeatherError::Driver(format!(
                    "device alias '{alias}' matches more than one Corsair hub; set unique_id"
                )));
            }
            let mut descriptor = descriptor(info);
            descriptor.details.insert("alias".into(), alias.clone());
            found.push(descriptor);
        }
        Ok(found)
    }

    pub(super) fn discover_all(&self) -> Result<Vec<DeviceDescriptor>> {
        let api = HidApi::new().map_err(hid_error("could not initialize HIDAPI"))?;
        Ok(api
            .device_list()
            .filter(|info| info.vendor_id() == 0x1b1c && info.product_id() == 0x0c3f)
            .map(descriptor)
            .collect())
    }

    pub(super) fn read_temperature(
        &mut self,
        alias: &str,
        config: &DeviceConfig,
        channel: u8,
    ) -> Result<f64> {
        let hub = self.hub(alias, config)?;
        hub.ensure_software_mode()?;
        hub.temperatures()?
            .into_iter()
            .find(|(current, _)| *current == channel)
            .map(|(_, value)| value)
            .ok_or_else(|| {
                FeatherError::Driver(format!(
                    "hub '{alias}' did not report temperature channel {channel}"
                ))
            })
    }

    pub(super) fn set_fans(
        &mut self,
        alias: &str,
        config: &DeviceConfig,
        channels: &[u32],
        percent: u8,
        expected_fan_count: Option<u8>,
    ) -> Result<FanWrite> {
        let hub = self.hub(alias, config)?;
        hub.ensure_software_mode()?;
        let topology = hub.devices()?;
        let speeds_before = hub.speeds()?;
        let selected = if channels.is_empty() {
            automatic_speed_channels(&topology.devices, &speeds_before)
        } else {
            channels
                .iter()
                .map(|channel| {
                    u8::try_from(*channel).map_err(|_| {
                        FeatherError::Driver(format!("fan channel {channel} is too large"))
                    })
                })
                .collect::<Result<BTreeSet<_>>>()?
        };
        if selected.is_empty() {
            return Err(FeatherError::Driver(format!(
                "hub '{alias}' reported no controllable fan channels"
            )));
        }
        let requested = selected
            .iter()
            .map(|channel| (*channel, percent))
            .collect::<BTreeMap<_, _>>();
        hub.set_speeds(&requested)?;
        let speeds_after = hub.speeds()?;
        Ok(fan_write_result(
            alias,
            percent,
            expected_fan_count,
            &topology,
            &selected,
            &speeds_after,
        ))
    }

    pub(super) fn set_rgb(
        &mut self,
        alias: &str,
        config: &DeviceConfig,
        led_count: u16,
        rgb: [u8; 3],
    ) -> Result<Value> {
        let hub = self.hub(alias, config)?;
        hub.ensure_software_mode()?;
        hub.set_rgb(led_count, rgb)?;
        Ok(json!({ "rgb": rgb, "led_count": led_count }))
    }

    pub(super) fn release(&mut self, alias: &str, config: &DeviceConfig) -> Result<()> {
        if !self.hubs.contains_key(alias) {
            let _ = self.hub(alias, config)?;
        }
        if let Some(mut hub) = self.hubs.remove(alias) {
            hub.hardware_mode()?;
        }
        Ok(())
    }

    pub(super) fn debug_dump(&mut self, alias: &str, config: &DeviceConfig) -> Result<String> {
        let hub = self.hub(alias, config)?;
        let was_software_mode = hub.software_mode;
        hub.ensure_software_mode()?;
        let expected_fan_count = match config {
            DeviceConfig::CorsairIcueLink {
                expected_fan_count, ..
            } => *expected_fan_count,
            DeviceConfig::NvidiaNvml { .. } => None,
        };
        let dump = hub.diagnostic_dump(expected_fan_count);
        if was_software_mode {
            return dump;
        }
        let restore = hub.hardware_mode();
        match (dump, restore) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(read_error), Err(restore_error)) => Err(FeatherError::Driver(format!(
                "{read_error}; hardware-mode restore also failed: {restore_error}"
            ))),
        }
    }

    pub(super) fn shutdown(&mut self) {
        for (alias, hub) in &mut self.hubs {
            if let Err(error) = hub.hardware_mode() {
                tracing::error!(%error, device = alias, "failed to restore Corsair hardware mode");
            }
        }
        self.hubs.clear();
    }

    fn hub(&mut self, alias: &str, config: &DeviceConfig) -> Result<&mut Hub> {
        if !self.hubs.contains_key(alias) {
            self.hubs.insert(alias.into(), Hub::open(config)?);
        }
        self.hubs
            .get_mut(alias)
            .ok_or_else(|| FeatherError::Driver(format!("hub '{alias}' could not be opened")))
    }
}

struct Hub {
    device: HidDevice,
    software_mode: bool,
    color_open: bool,
}

impl Hub {
    fn diagnostic_dump(&self, expected_fan_count: Option<u8>) -> Result<String> {
        let devices_response = self.read_endpoint(EP_DEVICES, DT_DEVICES)?;
        let speeds_response = self.read_endpoint(EP_SPEEDS, DT_SPEEDS)?;
        let temperatures_response = self.read_endpoint(EP_TEMPERATURES, DT_TEMPERATURES)?;
        let topology = parse_devices(&devices_response)?;
        let speeds = parse_speeds(&speeds_response)?;
        let speed_by_channel = speeds
            .iter()
            .map(|reading| (reading.channel, *reading))
            .collect::<BTreeMap<_, _>>();
        let fan_count = topology
            .devices
            .iter()
            .filter(|device| device_profile(device).is_some_and(|profile| profile.is_fan))
            .count();
        let speed_control_count = topology
            .devices
            .iter()
            .filter(|device| device_profile(device).is_some_and(|profile| profile.controls_speed))
            .count();
        let mut output = String::new();
        writeln!(
            output,
            "iCUE LINK topology: last_channel={} connected={} fans={} speed_controllable={}",
            topology.last_channel,
            topology.devices.len(),
            fan_count,
            speed_control_count
        )
        .map_err(diagnostic_format_error)?;
        if let Some(expected) = expected_fan_count {
            let health = if usize::from(expected) == fan_count {
                "healthy"
            } else {
                "degraded"
            };
            writeln!(
                output,
                "fan count: expected={expected} enumerated={fan_count} health={health}"
            )
            .map_err(diagnostic_format_error)?;
        }
        for device in &topology.devices {
            let (name, controls_speed, is_fan) = device_profile(device)
                .map_or(("unknown", false, false), |profile| {
                    (profile.name, profile.controls_speed, profile.is_fan)
                });
            let speed = speed_by_channel
                .get(&device.channel)
                .map_or_else(|| "not-reported".to_owned(), speed_description);
            writeln!(
                output,
                "  channel={} model=0x{:02x} variant=0x{:02x} name=\"{}\" id={} fan={} speed_control={} speed={}",
                device.channel,
                device.model,
                device.variant,
                name,
                device.id,
                is_fan,
                controls_speed,
                speed
            )
            .map_err(diagnostic_format_error)?;
        }
        let occupied = topology
            .devices
            .iter()
            .map(|device| device.channel)
            .collect::<BTreeSet<_>>();
        let empty_channels = (1..=topology.last_channel)
            .filter(|channel| !occupied.contains(channel))
            .collect::<Vec<_>>();
        writeln!(output, "empty channels: {empty_channels:?}\n")
            .map_err(diagnostic_format_error)?;

        write_endpoint_dump(
            &mut output,
            "DEVICES",
            EP_DEVICES,
            DT_DEVICES,
            "last_channel",
            topology.last_channel,
            &devices_response,
            7usize.saturating_add(topology.consumed),
        )?;
        let speed_count = speeds_response.get(6).copied().unwrap_or(0);
        write_endpoint_dump(
            &mut output,
            "SPEEDS",
            EP_SPEEDS,
            DT_SPEEDS,
            "count",
            speed_count,
            &speeds_response,
            7usize.saturating_add(usize::from(speed_count).saturating_mul(3)),
        )?;
        let temperature_count = temperatures_response.get(6).copied().unwrap_or(0);
        write_endpoint_dump(
            &mut output,
            "TEMPERATURES",
            EP_TEMPERATURES,
            DT_TEMPERATURES,
            "count",
            temperature_count,
            &temperatures_response,
            7usize.saturating_add(usize::from(temperature_count).saturating_mul(3)),
        )?;
        Ok(output)
    }

    fn open(config: &DeviceConfig) -> Result<Self> {
        let api = HidApi::new().map_err(hid_error("could not initialize HIDAPI"))?;
        let info = api
            .device_list()
            .find(|info| matches_device(info, config))
            .ok_or_else(|| FeatherError::Driver("configured Corsair hub was not found".into()))?;
        let device = api
            .open_path(info.path())
            .map_err(hid_error("could not open configured Corsair hub"))?;
        device
            .set_blocking_mode(true)
            .map_err(hid_error("could not configure Corsair HID reads"))?;
        Ok(Self {
            device,
            software_mode: false,
            color_open: false,
        })
    }

    fn ensure_software_mode(&mut self) -> Result<()> {
        if self.software_mode {
            return Ok(());
        }
        self.transfer(CMD_HARDWARE_MODE, &[])?;
        thread::sleep(Duration::from_millis(50));
        self.transfer(CMD_SOFTWARE_MODE, &[])?;
        self.software_mode = true;
        thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    fn hardware_mode(&mut self) -> Result<()> {
        if self.software_mode {
            self.transfer(CMD_HARDWARE_MODE, &[])?;
            self.software_mode = false;
            self.color_open = false;
        }
        Ok(())
    }

    fn transfer(&self, command: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; WRITE_SIZE];
        buffer[2] = 0x01;
        buffer[HEADER..HEADER + command.len()].copy_from_slice(command);
        let data_offset = HEADER + command.len();
        if data_offset + data.len() > buffer.len() {
            return Err(FeatherError::Driver(
                "Corsair HID packet is too large".into(),
            ));
        }
        buffer[data_offset..data_offset + data.len()].copy_from_slice(data);
        self.device
            .write(&buffer)
            .map_err(hid_error("Corsair HID write failed"))?;
        thread::sleep(Duration::from_millis(3));
        let mut response = vec![0u8; BUFFER_SIZE];
        let length = self
            .device
            .read_timeout(&mut response, 1_000)
            .map_err(hid_error("Corsair HID read failed"))?;
        if length == 0 {
            return Err(FeatherError::Driver(
                "Corsair HID response timed out".into(),
            ));
        }
        response.truncate(length);
        Ok(response)
    }

    fn read_endpoint(&self, endpoint: u8, data_type: &[u8]) -> Result<Vec<u8>> {
        self.transfer(CMD_CLOSE, &[endpoint])?;
        self.transfer(CMD_OPEN, &[endpoint])?;
        let mut response = self.transfer(CMD_READ, &[endpoint])?;
        if response.get(4..6) == Some(data_type) {
            let continuation = self.transfer(CMD_READ, &[endpoint])?;
            if continuation.len() > 4 {
                response.extend_from_slice(&continuation[4..]);
            }
        }
        self.transfer(CMD_CLOSE, &[endpoint])?;
        Ok(response)
    }

    fn write_endpoint(&self, endpoint: u8, data_type: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        let mut data = vec![0u8; WRITE_HEADER + data_type.len() + payload.len()];
        let length = u16::try_from(payload.len() + data_type.len())
            .map_err(|_| FeatherError::Driver("Corsair endpoint payload is too large".into()))?;
        data[0..2].copy_from_slice(&length.to_le_bytes());
        data[WRITE_HEADER..WRITE_HEADER + data_type.len()].copy_from_slice(data_type);
        data[WRITE_HEADER + data_type.len()..].copy_from_slice(payload);
        self.transfer(CMD_CLOSE, &[endpoint])?;
        self.transfer(CMD_OPEN, &[endpoint])?;
        let response = self.transfer(CMD_WRITE, &data)?;
        self.transfer(CMD_CLOSE, &[endpoint])?;
        Ok(response)
    }

    fn temperatures(&self) -> Result<Vec<(u8, f64)>> {
        let response = self.read_endpoint(EP_TEMPERATURES, DT_TEMPERATURES)?;
        parse_temperatures(&response)
    }

    fn devices(&self) -> Result<LinkTopology> {
        let response = self.read_endpoint(EP_DEVICES, DT_DEVICES)?;
        parse_devices(&response)
    }

    fn speeds(&self) -> Result<Vec<SpeedReading>> {
        let response = self.read_endpoint(EP_SPEEDS, DT_SPEEDS)?;
        parse_speeds(&response)
    }

    fn set_speeds(&self, speeds: &BTreeMap<u8, u8>) -> Result<()> {
        let mut payload = vec![0u8; speeds.len() * 4 + 1];
        payload[0] = speeds
            .len()
            .try_into()
            .map_err(|_| FeatherError::Driver("too many Corsair fan channels".into()))?;
        for (index, (channel, percent)) in speeds.iter().enumerate() {
            let offset = 1 + index * 4;
            payload[offset] = *channel;
            payload[offset + 1] = 0;
            payload[offset + 2] = (*percent).min(100);
        }
        let mut last_status = None;
        for attempt in 0..=10 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(100));
            }
            let response = self.write_endpoint(EP_SET_SPEED, DT_SET_SPEED, &payload)?;
            last_status = response.get(3).copied();
            if last_status == Some(0) {
                return Ok(());
            }
        }
        Err(FeatherError::Driver(format!(
            "hub rejected fan speed write with status {}",
            last_status.map_or_else(|| "missing".into(), |status| format!("0x{status:02x}"))
        )))
    }

    fn set_rgb(&mut self, led_count: u16, rgb: [u8; 3]) -> Result<()> {
        if !self.color_open {
            self.transfer(CMD_CLOSE_COLOR, &[EP_SET_COLOR])?;
            self.transfer(CMD_OPEN_COLOR, &[EP_SET_COLOR])?;
            self.color_open = true;
        }
        let mut rgb_data = Vec::with_capacity(usize::from(led_count) * 3);
        for _ in 0..led_count {
            rgb_data.extend_from_slice(&rgb);
        }
        let mut data = vec![0u8; WRITE_HEADER + DT_SET_COLOR.len() + rgb_data.len()];
        let length = u16::try_from(rgb_data.len() + DT_SET_COLOR.len())
            .map_err(|_| FeatherError::Driver("Corsair RGB payload is too large".into()))?;
        data[0..2].copy_from_slice(&length.to_le_bytes());
        data[WRITE_HEADER..WRITE_HEADER + DT_SET_COLOR.len()].copy_from_slice(DT_SET_COLOR);
        data[WRITE_HEADER + DT_SET_COLOR.len()..].copy_from_slice(&rgb_data);
        let response = self.transfer(CMD_WRITE_COLOR, &data)?;
        if response.get(3) == Some(&0) {
            Ok(())
        } else {
            Err(FeatherError::Driver(format!(
                "hub rejected RGB write with status {}",
                response
                    .get(3)
                    .map_or_else(|| "missing".into(), |status| format!("0x{status:02x}"))
            )))
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.hardware_mode();
    }
}

fn matches_device(info: &hidapi::DeviceInfo, config: &DeviceConfig) -> bool {
    let DeviceConfig::CorsairIcueLink {
        vendor_id,
        product_id,
        unique_id,
        interface,
        ..
    } = config
    else {
        return false;
    };
    info.vendor_id() == *vendor_id
        && info.product_id() == *product_id
        && info.interface_number() == *interface
        && unique_id.as_ref().is_none_or(|expected| {
            info.serial_number()
                .is_some_and(|serial| serial.eq_ignore_ascii_case(expected))
        })
}

fn descriptor(info: &hidapi::DeviceInfo) -> DeviceDescriptor {
    let serial = info.serial_number().unwrap_or("no-serial");
    let mut details = BTreeMap::new();
    details.insert("vendor_id".into(), format!("{:04x}", info.vendor_id()));
    details.insert("product_id".into(), format!("{:04x}", info.product_id()));
    details.insert("unique_id".into(), serial.into());
    details.insert("interface".into(), info.interface_number().to_string());
    details.insert("path".into(), info.path().to_string_lossy().into_owned());
    DeviceDescriptor {
        id: format!(
            "hid:{:04x}:{:04x}:{serial}:{}",
            info.vendor_id(),
            info.product_id(),
            info.interface_number()
        ),
        driver: "corsair-icue-link".into(),
        name: info
            .product_string()
            .unwrap_or("Corsair iCUE LINK System Hub")
            .into(),
        details,
    }
}

fn automatic_speed_channels(devices: &[LinkDevice], speeds: &[SpeedReading]) -> BTreeSet<u8> {
    let mut channels = devices
        .iter()
        .filter(|device| device_profile(device).is_some_and(|profile| profile.controls_speed))
        .map(|device| device.channel)
        .collect::<BTreeSet<_>>();
    channels.extend(
        speeds
            .iter()
            .filter(|reading| reading.rpm.is_some())
            .map(|reading| reading.channel),
    );
    channels
}

fn fan_write_result(
    alias: &str,
    percent: u8,
    expected_fan_count: Option<u8>,
    topology: &LinkTopology,
    selected: &BTreeSet<u8>,
    speeds: &[SpeedReading],
) -> FanWrite {
    let fan_channels = topology
        .devices
        .iter()
        .filter(|device| device_profile(device).is_some_and(|profile| profile.is_fan))
        .map(|device| device.channel)
        .collect::<Vec<_>>();
    let by_channel = speeds
        .iter()
        .map(|reading| (reading.channel, reading))
        .collect::<BTreeMap<_, _>>();
    let mut rpm = BTreeMap::new();
    let mut unavailable = BTreeMap::new();
    let mut missing = Vec::new();
    let mut stalled = Vec::new();
    for channel in selected {
        match by_channel.get(channel) {
            Some(SpeedReading {
                status: 0,
                rpm: Some(value),
                ..
            }) => {
                rpm.insert(channel.to_string(), *value);
                if *value <= 0 {
                    stalled.push(*channel);
                }
            }
            Some(reading) => {
                unavailable.insert(channel.to_string(), format!("0x{:02x}", reading.status));
            }
            None => missing.push(*channel),
        }
    }

    let mut warnings = Vec::new();
    if let Some(expected) = expected_fan_count
        && usize::from(expected) != fan_channels.len()
    {
        warnings.push(format!(
            "hub '{alias}' enumerated {} fan subdevices; expected {expected}",
            fan_channels.len()
        ));
    }
    if !unavailable.is_empty() {
        warnings.push(format!(
            "hub '{alias}' reports unavailable speed channels {}",
            unavailable.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !missing.is_empty() {
        warnings.push(format!(
            "hub '{alias}' omitted selected speed channels {}",
            comma_separated_channels(&missing)
        ));
    }
    if !stalled.is_empty() {
        warnings.push(format!(
            "hub '{alias}' reports zero RPM on channels {}",
            comma_separated_channels(&stalled)
        ));
    }

    let mut observed = json!({
        "percent": percent,
        "channels": selected,
        "fan_channels": fan_channels,
        "rpm": rpm,
        "unavailable": unavailable,
        "missing": missing,
        "stalled": stalled,
    });
    if let Some(expected) = expected_fan_count
        && let Some(object) = observed.as_object_mut()
    {
        object.insert("expected_fan_count".into(), json!(expected));
    }
    FanWrite {
        observed,
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
    }
}

fn comma_separated_channels(channels: &[u8]) -> String {
    channels
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn device_profile(device: &LinkDevice) -> Option<LinkDeviceProfile> {
    let profile = match device.model {
        0x01 => LinkDeviceProfile {
            name: "QX Fan",
            controls_speed: true,
            is_fan: true,
        },
        0x02 => LinkDeviceProfile {
            name: "LX Fan",
            controls_speed: true,
            is_fan: true,
        },
        0x03 => LinkDeviceProfile {
            name: "RX MAX RGB Fan",
            controls_speed: true,
            is_fan: true,
        },
        0x04 => LinkDeviceProfile {
            name: "RX MAX Fan",
            controls_speed: true,
            is_fan: true,
        },
        0x05 => LinkDeviceProfile {
            name: "iCUE LINK Adapter",
            controls_speed: false,
            is_fan: false,
        },
        0x07 => LinkDeviceProfile {
            name: "H-series liquid cooler",
            controls_speed: true,
            is_fan: false,
        },
        0x09 => LinkDeviceProfile {
            name: "XC7 water block",
            controls_speed: false,
            is_fan: false,
        },
        0x0a => LinkDeviceProfile {
            name: "XG3 water block",
            controls_speed: true,
            is_fan: false,
        },
        0x0b => LinkDeviceProfile {
            name: "HXi SHIFT PSU",
            controls_speed: true,
            is_fan: false,
        },
        0x0c => LinkDeviceProfile {
            name: "XD5 pump",
            controls_speed: true,
            is_fan: false,
        },
        0x0f => LinkDeviceProfile {
            name: "RX RGB Fan",
            controls_speed: true,
            is_fan: true,
        },
        0x10 => LinkDeviceProfile {
            name: "VRM Fan CapSwap Module",
            controls_speed: true,
            is_fan: true,
        },
        0x11 => LinkDeviceProfile {
            name: "TITAN AIO",
            controls_speed: true,
            is_fan: false,
        },
        0x13 => LinkDeviceProfile {
            name: "RX Fan",
            controls_speed: true,
            is_fan: true,
        },
        0x19 => LinkDeviceProfile {
            name: "XD6 pump",
            controls_speed: true,
            is_fan: false,
        },
        0x1b => LinkDeviceProfile {
            name: "COMMANDER DUO",
            controls_speed: true,
            is_fan: false,
        },
        _ => return None,
    };
    Some(profile)
}

fn parse_devices(response: &[u8]) -> Result<LinkTopology> {
    validate_response(response, DT_DEVICES)?;
    let last_channel = response[6];
    let data = &response[7..];
    let mut devices = Vec::new();
    let mut cursor = 0usize;
    for channel in 1..=last_channel {
        let header_end = cursor.saturating_add(8);
        let header = data.get(cursor..header_end).ok_or_else(|| {
            FeatherError::Driver(format!(
                "truncated Corsair device record for channel {channel}"
            ))
        })?;
        let id_length = usize::from(header[7]);
        if id_length == 0 {
            cursor = header_end;
            continue;
        }
        let record_end = header_end.saturating_add(id_length);
        let id_bytes = data.get(header_end..record_end).ok_or_else(|| {
            FeatherError::Driver(format!("truncated Corsair device ID for channel {channel}"))
        })?;
        let id_end = id_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(id_bytes.len());
        devices.push(LinkDevice {
            channel,
            model: header[2],
            variant: header[3],
            id: String::from_utf8_lossy(&id_bytes[..id_end]).into_owned(),
        });
        cursor = record_end;
    }
    Ok(LinkTopology {
        last_channel,
        devices,
        consumed: cursor,
    })
}

fn speed_description(reading: &SpeedReading) -> String {
    match reading.rpm {
        Some(rpm) => format!("available({rpm} rpm)"),
        None => format!("unavailable(status=0x{:02x})", reading.status),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_endpoint_dump(
    output: &mut String,
    name: &str,
    endpoint: u8,
    data_type: &[u8],
    count_name: &str,
    count: u8,
    response: &[u8],
    meaningful_length: usize,
) -> Result<()> {
    writeln!(
        output,
        "{name} endpoint=0x{endpoint:02x} data_type={} {count_name}={count}",
        hex(data_type)
    )
    .map_err(diagnostic_format_error)?;
    let length = meaningful_length.min(response.len());
    for (offset, row) in response[..length].chunks(16).enumerate() {
        write!(output, "  {:04x}: ", offset * 16).map_err(diagnostic_format_error)?;
        for byte in row {
            write!(output, "{byte:02x} ").map_err(diagnostic_format_error)?;
        }
        output.push('\n');
    }
    output.push('\n');
    Ok(())
}

fn diagnostic_format_error(_error: std::fmt::Error) -> FeatherError {
    FeatherError::Driver("could not format Corsair diagnostics".into())
}

fn parse_temperatures(response: &[u8]) -> Result<Vec<(u8, f64)>> {
    validate_response(response, DT_TEMPERATURES)?;
    let count = usize::from(response[6]);
    let data = &response[7..];
    let mut temperatures = Vec::new();
    for channel in 0..count {
        let offset = channel * 3;
        if offset + 2 >= data.len() {
            break;
        }
        if data[offset] == 0 {
            let raw = i16::from_le_bytes([data[offset + 1], data[offset + 2]]);
            let temperature = f64::from(raw) / 10.0;
            if (1.0..90.0).contains(&temperature) {
                let channel = u8::try_from(channel).map_err(|_| {
                    FeatherError::Driver("Corsair temperature channel is too large".into())
                })?;
                temperatures.push((channel, temperature));
            }
        }
    }
    Ok(temperatures)
}

fn parse_speeds(response: &[u8]) -> Result<Vec<SpeedReading>> {
    validate_response(response, DT_SPEEDS)?;
    let count = usize::from(response[6]);
    let data = &response[7..];
    let expected_length = count.saturating_mul(3);
    if data.len() < expected_length {
        return Err(FeatherError::Driver(format!(
            "truncated Corsair speed data (expected {expected_length} bytes, got {})",
            data.len()
        )));
    }
    let mut speeds = Vec::with_capacity(count);
    for channel in 0..count {
        let offset = channel * 3;
        let status = data[offset];
        let channel = u8::try_from(channel)
            .map_err(|_| FeatherError::Driver("Corsair fan channel is too large".into()))?;
        speeds.push(SpeedReading {
            channel,
            status,
            rpm: (status == 0).then(|| i16::from_le_bytes([data[offset + 1], data[offset + 2]])),
        });
    }
    Ok(speeds)
}

fn validate_response(response: &[u8], data_type: &[u8]) -> Result<()> {
    if response.len() < 8 {
        return Err(FeatherError::Driver(format!(
            "short Corsair response ({} bytes)",
            response.len()
        )));
    }
    if response[3] != 0 {
        return Err(FeatherError::Driver(format!(
            "Corsair response status 0x{:02x}",
            response[3]
        )));
    }
    if response.get(4..6) != Some(data_type) {
        return Err(FeatherError::Driver(format!(
            "unexpected Corsair data type {}",
            hex(response.get(4..6).unwrap_or_default())
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hid_error(context: &'static str) -> impl FnOnce(hidapi::HidError) -> FeatherError {
    move |error| FeatherError::Driver(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(data_type: &[u8], values: &[(u8, i16)]) -> Vec<u8> {
        let mut response = vec![0; 7 + values.len() * 3];
        response[3] = 0;
        response[4..6].copy_from_slice(data_type);
        response[6] = u8::try_from(values.len()).unwrap_or(u8::MAX);
        for (index, (valid, value)) in values.iter().enumerate() {
            let offset = 7 + index * 3;
            response[offset] = *valid;
            response[offset + 1..offset + 3].copy_from_slice(&value.to_le_bytes());
        }
        response
    }

    fn devices_response(records: &[Option<(u8, u8, &str)>]) -> Result<Vec<u8>> {
        let mut response = vec![0; 7];
        response[4..6].copy_from_slice(DT_DEVICES);
        response[6] = u8::try_from(records.len())
            .map_err(|_| FeatherError::Driver("test fixture has too many devices".into()))?;
        for record in records {
            match record {
                Some((model, variant, id)) => {
                    let id_length = u8::try_from(id.len()).map_err(|_| {
                        FeatherError::Driver("test fixture device ID is too long".into())
                    })?;
                    response.extend_from_slice(&[0, 0, *model, *variant, 0, 0, 0, id_length]);
                    response.extend_from_slice(id.as_bytes());
                }
                None => response.extend_from_slice(&[0; 8]),
            }
        }
        Ok(response)
    }

    #[test]
    fn parses_temperature_fixture() -> Result<()> {
        let fixture = response(DT_TEMPERATURES, &[(0, 425), (1, 700), (0, 615)]);
        assert_eq!(parse_temperatures(&fixture)?, vec![(0, 42.5), (2, 61.5)]);
        Ok(())
    }

    #[test]
    fn parses_speed_fixture() -> Result<()> {
        let fixture = response(DT_SPEEDS, &[(0, 900), (1, 0), (0, 1_450)]);
        assert_eq!(
            parse_speeds(&fixture)?,
            vec![
                SpeedReading {
                    channel: 0,
                    status: 0,
                    rpm: Some(900),
                },
                SpeedReading {
                    channel: 1,
                    status: 1,
                    rpm: None,
                },
                SpeedReading {
                    channel: 2,
                    status: 0,
                    rpm: Some(1_450),
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn parses_link_devices_with_empty_channels() -> Result<()> {
        let fixture = devices_response(&[
            None,
            Some((0x04, 0x00, "FIRST")),
            Some((0x04, 0x00, "SECOND")),
            None,
        ])?;

        let topology = parse_devices(&fixture)?;

        assert_eq!(topology.last_channel, 4);
        assert_eq!(
            topology.devices,
            vec![
                LinkDevice {
                    channel: 2,
                    model: 0x04,
                    variant: 0,
                    id: "FIRST".into(),
                },
                LinkDevice {
                    channel: 3,
                    model: 0x04,
                    variant: 0,
                    id: "SECOND".into(),
                },
            ]
        );
        assert_eq!(topology.consumed, 43);
        Ok(())
    }

    #[test]
    fn rejects_a_truncated_link_device_record() -> anyhow::Result<()> {
        let mut fixture = devices_response(&[Some((0x04, 0x00, "FAN"))])?;
        fixture.pop();

        let Err(error) = parse_devices(&fixture) else {
            anyhow::bail!("truncated device record was accepted");
        };
        assert!(error.to_string().contains("truncated Corsair device ID"));
        Ok(())
    }

    #[test]
    fn automatic_selection_keeps_enumerated_unavailable_fans() {
        let devices = vec![
            LinkDevice {
                channel: 2,
                model: 0x04,
                variant: 0,
                id: "FIRST".into(),
            },
            LinkDevice {
                channel: 5,
                model: 0x04,
                variant: 0,
                id: "SECOND".into(),
            },
            LinkDevice {
                channel: 13,
                model: 0x05,
                variant: 1,
                id: "ADAPTER".into(),
            },
        ];
        let speeds = vec![
            SpeedReading {
                channel: 2,
                status: 0,
                rpm: Some(800),
            },
            SpeedReading {
                channel: 5,
                status: 1,
                rpm: None,
            },
            SpeedReading {
                channel: 8,
                status: 0,
                rpm: Some(900),
            },
            SpeedReading {
                channel: 13,
                status: 1,
                rpm: None,
            },
        ];

        assert_eq!(
            automatic_speed_channels(&devices, &speeds),
            BTreeSet::from([2, 5, 8])
        );
    }

    #[test]
    fn expected_fan_mismatch_returns_a_degraded_warning() {
        let topology = LinkTopology {
            last_channel: 2,
            devices: vec![LinkDevice {
                channel: 2,
                model: 0x04,
                variant: 0,
                id: "ONLY".into(),
            }],
            consumed: 0,
        };
        let speeds = [SpeedReading {
            channel: 2,
            status: 0,
            rpm: Some(800),
        }];

        let write = fan_write_result("hub", 40, Some(2), &topology, &BTreeSet::from([2]), &speeds);

        assert_eq!(write.observed["expected_fan_count"], 2);
        assert_eq!(write.observed["fan_channels"], json!([2]));
        assert!(
            write
                .warning
                .is_some_and(|warning| warning.contains("enumerated 1 fan subdevices; expected 2"))
        );
    }

    #[test]
    fn rejects_wrong_data_type() {
        let fixture = response(&[0xff, 0xff], &[(0, 900)]);
        assert!(parse_speeds(&fixture).is_err());
    }
}
