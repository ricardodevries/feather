//! Thermal Grizzly WireView Pro II discovery and display control over USB serial.

use std::{collections::BTreeMap, thread, time::Duration};

use feather_core::{
    config::DeviceConfig,
    error::{FeatherError, Result},
    types::DeviceDescriptor,
};
use serde_json::{Value, json};
use serialport::{
    ClearBuffer, DataBits, FlowControl, Parity, SerialPort, SerialPortType, StopBits,
};

use super::DisplayTarget;

const VENDOR_ID: u16 = 0x0483;
const PRODUCT_ID: u16 = 0x5740;
const BAUD_RATE: u32 = 115_200;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const RTS_SETTLE_TIME: Duration = Duration::from_millis(10);
const WELCOME_MESSAGE: &[u8] = b"Thermal Grizzly WireView Pro II\0";

const CMD_READ_VENDOR_DATA: u8 = 0x01;
const CMD_READ_CONFIG: u8 = 0x05;
const CMD_WRITE_CONFIG: u8 = 0x06;
const CMD_SCREEN_CHANGE: u8 = 0x0c;
const SCREEN_GOTO_SAME: u8 = 0xef;

const CONFIG_VERSION_OFFSET: usize = 2;
const BACKLIGHT_DUTY_OFFSET: usize = 44;
const CONFIG_FRAME_PAYLOAD: usize = 62;
const TIMEOUT_MODE_STATIC: u8 = 0;
const TIMEOUT_MODE_SLEEP: u8 = 2;
const AWAKE_TIMEOUT_SECONDS: u8 = 30;
const SLEEP_TIMEOUT_SECONDS: u8 = 5;
const WIREVIEW_VENDOR_ID: u8 = 0xef;
const WIREVIEW_PRODUCT_ID: u8 = 0x05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisplaySettings {
    brightness_percent: u8,
    timeout_mode: u8,
    timeout_seconds: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisplayOffsets {
    timeout_mode: usize,
    timeout_seconds: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WireViewPort {
    path: String,
    serial: String,
    manufacturer: Option<String>,
    product: Option<String>,
}

pub(super) struct WireViewDriver;

impl WireViewDriver {
    pub(super) const fn new() -> Self {
        Self
    }

    pub(super) fn discover_configured(
        &self,
        configured: &BTreeMap<String, DeviceConfig>,
    ) -> Result<Vec<DeviceDescriptor>> {
        if !configured
            .values()
            .any(|config| matches!(config, DeviceConfig::WireViewProIi { .. }))
        {
            return Ok(Vec::new());
        }
        let ports = enumerate()?;
        let mut found = Vec::new();
        for (alias, config) in configured {
            let DeviceConfig::WireViewProIi { serial } = config else {
                continue;
            };
            if let Some(port) = find_port(&ports, serial) {
                let mut descriptor = descriptor(port);
                descriptor.details.insert("alias".into(), alias.clone());
                found.push(descriptor);
            }
        }
        Ok(found)
    }

    pub(super) fn discover_all(&self) -> Result<Vec<DeviceDescriptor>> {
        Ok(enumerate()?.iter().map(descriptor).collect())
    }

    pub(super) fn set_display(
        &self,
        alias: &str,
        config: &DeviceConfig,
        target: DisplayTarget,
    ) -> Result<Value> {
        let serial = serial(config)?;
        let ports = enumerate()?;
        let port = find_port(&ports, serial).ok_or_else(|| {
            FeatherError::Driver(format!(
                "WireView device '{alias}' with serial '{serial}' is unavailable"
            ))
        })?;
        let mut serial_port = open(&port.path, alias)?;
        read_welcome(serial_port.as_mut(), alias)?;
        let firmware_version = verify_device(serial_port.as_mut(), alias)?;
        let mut device_config = read_config(serial_port.as_mut(), alias)?;
        let previous = display_settings(&device_config)?;
        let desired = desired_display_settings(target)?;
        if previous != desired {
            set_config_display(&mut device_config, desired)?;
            write_config(serial_port.as_mut(), alias, &device_config)?;
            repaint(serial_port.as_mut(), alias)?;
        }
        let state = match target {
            DisplayTarget::Awake { .. } => "awake",
            DisplayTarget::Sleep => "sleep",
        };
        Ok(json!({
            "state": state,
            "brightness_percent": desired.brightness_percent,
            "timeout_mode": timeout_mode_name(desired.timeout_mode)?,
            "timeout_seconds": desired.timeout_seconds,
            "previous_brightness_percent": previous.brightness_percent,
            "previous_timeout_mode": timeout_mode_name(previous.timeout_mode)?,
            "previous_timeout_seconds": previous.timeout_seconds,
            "serial": serial,
            "firmware_version": firmware_version,
        }))
    }
}

fn read_welcome(port: &mut dyn SerialPort, alias: &str) -> Result<()> {
    clear_input(port, alias)?;
    port.write_request_to_send(true).map_err(|error| {
        FeatherError::Driver(format!(
            "could not start WireView device '{alias}' handshake: {error}"
        ))
    })?;
    thread::sleep(RTS_SETTLE_TIME);

    let mut welcome = vec![0_u8; WELCOME_MESSAGE.len()];
    let read_result = port.read_exact(&mut welcome);
    thread::sleep(RTS_SETTLE_TIME);
    let rts_result = port.write_request_to_send(false);

    read_result.map_err(|error| io_error(alias, "read welcome message", error))?;
    rts_result.map_err(|error| {
        FeatherError::Driver(format!(
            "could not finish WireView device '{alias}' handshake: {error}"
        ))
    })?;
    validate_welcome(alias, &welcome)
}

fn validate_welcome(alias: &str, welcome: &[u8]) -> Result<()> {
    if welcome == WELCOME_MESSAGE {
        return Ok(());
    }
    let prefix = welcome
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    Err(FeatherError::Driver(format!(
        "serial device for '{alias}' returned an unexpected welcome message starting with {prefix}"
    )))
}

fn enumerate() -> Result<Vec<WireViewPort>> {
    let ports = serialport::available_ports().map_err(|error| {
        FeatherError::Driver(format!("could not enumerate serial ports: {error}"))
    })?;
    Ok(ports
        .into_iter()
        .filter_map(|port| {
            let SerialPortType::UsbPort(info) = port.port_type else {
                return None;
            };
            if info.vid != VENDOR_ID || info.pid != PRODUCT_ID {
                return None;
            }
            let serial = info.serial_number?.trim().to_owned();
            if serial.is_empty() {
                return None;
            }
            Some(WireViewPort {
                path: port.port_name,
                serial,
                manufacturer: info.manufacturer,
                product: info.product,
            })
        })
        .collect())
}

fn find_port<'a>(ports: &'a [WireViewPort], serial: &str) -> Option<&'a WireViewPort> {
    ports
        .iter()
        .find(|port| port.serial.eq_ignore_ascii_case(serial))
}

fn descriptor(port: &WireViewPort) -> DeviceDescriptor {
    let mut details = BTreeMap::new();
    details.insert("serial".into(), port.serial.clone());
    details.insert("port".into(), port.path.clone());
    details.insert("vendor_id".into(), format!("{VENDOR_ID:04x}"));
    details.insert("product_id".into(), format!("{PRODUCT_ID:04x}"));
    if let Some(manufacturer) = &port.manufacturer {
        details.insert("manufacturer".into(), manufacturer.clone());
    }
    if let Some(product) = &port.product {
        details.insert("product".into(), product.clone());
    }
    DeviceDescriptor {
        id: format!("wireview-pro-ii:{}", port.serial),
        driver: "wire-view-pro-ii".into(),
        name: "Thermal Grizzly WireView Pro II".into(),
        details,
    }
}

fn serial(config: &DeviceConfig) -> Result<&str> {
    match config {
        DeviceConfig::WireViewProIi { serial } => Ok(serial),
        _ => Err(FeatherError::Driver(
            "WireView driver received another device type".into(),
        )),
    }
}

fn open(path: &str, alias: &str) -> Result<Box<dyn SerialPort>> {
    serialport::new(path, BAUD_RATE)
        .data_bits(DataBits::Eight)
        .flow_control(FlowControl::None)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .timeout(IO_TIMEOUT)
        .open()
        .map_err(|error| {
            FeatherError::Driver(format!(
                "could not open WireView device '{alias}' at {path}: {error}"
            ))
        })
}

fn verify_device(port: &mut dyn SerialPort, alias: &str) -> Result<u8> {
    clear_input(port, alias)?;
    port.write_all(&[CMD_READ_VENDOR_DATA])
        .map_err(|error| io_error(alias, "request device identity", error))?;
    port.flush()
        .map_err(|error| io_error(alias, "flush device identity request", error))?;
    let mut identity = [0_u8; 3];
    port.read_exact(&mut identity)
        .map_err(|error| io_error(alias, "read device identity", error))?;
    if identity[0] != WIREVIEW_VENDOR_ID || identity[1] != WIREVIEW_PRODUCT_ID {
        return Err(FeatherError::Driver(format!(
            "serial device for '{alias}' returned an unexpected identity {:02x}:{:02x}",
            identity[0], identity[1]
        )));
    }
    Ok(identity[2])
}

fn read_config(port: &mut dyn SerialPort, alias: &str) -> Result<Vec<u8>> {
    clear_input(port, alias)?;
    port.write_all(&[CMD_READ_CONFIG])
        .map_err(|error| io_error(alias, "request configuration", error))?;
    port.flush()
        .map_err(|error| io_error(alias, "flush configuration request", error))?;

    let mut prefix = [0_u8; 4];
    port.read_exact(&mut prefix)
        .map_err(|error| io_error(alias, "read configuration header", error))?;
    let size = config_size(prefix[CONFIG_VERSION_OFFSET])?;
    let mut config = vec![0_u8; size];
    config[..prefix.len()].copy_from_slice(&prefix);
    port.read_exact(&mut config[prefix.len()..])
        .map_err(|error| io_error(alias, "read configuration", error))?;
    Ok(config)
}

fn write_config(port: &mut dyn SerialPort, alias: &str, config: &[u8]) -> Result<()> {
    clear_input(port, alias)?;
    for frame in config_frames(config)? {
        port.write_all(&frame)
            .map_err(|error| io_error(alias, "write configuration", error))?;
    }
    port.flush()
        .map_err(|error| io_error(alias, "flush configuration", error))
}

fn config_frames(config: &[u8]) -> Result<Vec<Vec<u8>>> {
    config
        .chunks(CONFIG_FRAME_PAYLOAD)
        .enumerate()
        .map(|(index, payload)| {
            let byte_offset = index
                .checked_mul(CONFIG_FRAME_PAYLOAD)
                .and_then(|value| u8::try_from(value).ok())
                .ok_or_else(|| {
                    FeatherError::Driver("WireView configuration is too large".into())
                })?;
            let mut frame = Vec::with_capacity(payload.len() + 2);
            frame.push(CMD_WRITE_CONFIG);
            frame.push(byte_offset);
            frame.extend_from_slice(payload);
            Ok(frame)
        })
        .collect()
}

fn repaint(port: &mut dyn SerialPort, alias: &str) -> Result<()> {
    port.write_all(&[CMD_SCREEN_CHANGE, SCREEN_GOTO_SAME])
        .map_err(|error| io_error(alias, "refresh display", error))?;
    port.flush()
        .map_err(|error| io_error(alias, "flush display refresh", error))
}

fn clear_input(port: &mut dyn SerialPort, alias: &str) -> Result<()> {
    port.clear(ClearBuffer::Input).map_err(|error| {
        FeatherError::Driver(format!(
            "could not clear WireView device '{alias}' input: {error}"
        ))
    })
}

fn io_error(alias: &str, action: &str, error: std::io::Error) -> FeatherError {
    FeatherError::Driver(format!(
        "could not {action} for WireView device '{alias}': {error}"
    ))
}

fn config_size(version: u8) -> Result<usize> {
    match version {
        0 => Ok(72),
        1 => Ok(74),
        2 => Ok(96),
        other => Err(FeatherError::Driver(format!(
            "unsupported WireView configuration version {other}"
        ))),
    }
}

fn display_offsets(version: u8) -> Result<DisplayOffsets> {
    match version {
        0 => Ok(DisplayOffsets {
            timeout_mode: 68,
            timeout_seconds: 71,
        }),
        1 => Ok(DisplayOffsets {
            timeout_mode: 69,
            timeout_seconds: 72,
        }),
        2 => Ok(DisplayOffsets {
            timeout_mode: 72,
            timeout_seconds: 75,
        }),
        other => Err(FeatherError::Driver(format!(
            "unsupported WireView configuration version {other}"
        ))),
    }
}

fn display_settings(config: &[u8]) -> Result<DisplaySettings> {
    let version = config.get(CONFIG_VERSION_OFFSET).copied().ok_or_else(|| {
        FeatherError::Driver("WireView configuration did not contain a version".into())
    })?;
    let offsets = display_offsets(version)?;
    let settings = DisplaySettings {
        brightness_percent: config.get(BACKLIGHT_DUTY_OFFSET).copied().ok_or_else(|| {
            FeatherError::Driver("WireView configuration did not contain backlight duty".into())
        })?,
        timeout_mode: config.get(offsets.timeout_mode).copied().ok_or_else(|| {
            FeatherError::Driver("WireView configuration did not contain timeout mode".into())
        })?,
        timeout_seconds: config
            .get(offsets.timeout_seconds)
            .copied()
            .ok_or_else(|| {
                FeatherError::Driver(
                    "WireView configuration did not contain timeout interval".into(),
                )
            })?,
    };
    if settings.brightness_percent > 100 {
        return Err(FeatherError::Driver(format!(
            "WireView returned invalid backlight duty {}%",
            settings.brightness_percent
        )));
    }
    timeout_mode_name(settings.timeout_mode)?;
    Ok(settings)
}

fn desired_display_settings(target: DisplayTarget) -> Result<DisplaySettings> {
    let settings = match target {
        DisplayTarget::Awake { brightness_percent } => DisplaySettings {
            brightness_percent,
            timeout_mode: TIMEOUT_MODE_STATIC,
            timeout_seconds: AWAKE_TIMEOUT_SECONDS,
        },
        DisplayTarget::Sleep => DisplaySettings {
            brightness_percent: 0,
            timeout_mode: TIMEOUT_MODE_SLEEP,
            timeout_seconds: SLEEP_TIMEOUT_SECONDS,
        },
    };
    if settings.brightness_percent > 100 {
        return Err(FeatherError::Driver(format!(
            "WireView brightness {}% is over 100%",
            settings.brightness_percent
        )));
    }
    Ok(settings)
}

fn set_config_display(config: &mut [u8], settings: DisplaySettings) -> Result<()> {
    let version = config.get(CONFIG_VERSION_OFFSET).copied().ok_or_else(|| {
        FeatherError::Driver("WireView configuration did not contain a version".into())
    })?;
    let offsets = display_offsets(version)?;
    timeout_mode_name(settings.timeout_mode)?;
    if settings.brightness_percent > 100 {
        return Err(FeatherError::Driver(format!(
            "WireView brightness {}% is over 100%",
            settings.brightness_percent
        )));
    }
    *config.get_mut(BACKLIGHT_DUTY_OFFSET).ok_or_else(|| {
        FeatherError::Driver("WireView configuration did not contain backlight duty".into())
    })? = settings.brightness_percent;
    *config.get_mut(offsets.timeout_mode).ok_or_else(|| {
        FeatherError::Driver("WireView configuration did not contain timeout mode".into())
    })? = settings.timeout_mode;
    *config.get_mut(offsets.timeout_seconds).ok_or_else(|| {
        FeatherError::Driver("WireView configuration did not contain timeout interval".into())
    })? = settings.timeout_seconds;
    Ok(())
}

fn timeout_mode_name(mode: u8) -> Result<&'static str> {
    match mode {
        TIMEOUT_MODE_STATIC => Ok("static"),
        1 => Ok("cycle"),
        TIMEOUT_MODE_SLEEP => Ok("sleep"),
        other => Err(FeatherError::Driver(format!(
            "WireView returned invalid timeout mode {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_known_configuration_sizes() -> anyhow::Result<()> {
        assert_eq!(config_size(0)?, 72);
        assert_eq!(config_size(1)?, 74);
        assert_eq!(config_size(2)?, 96);
        assert!(config_size(3).is_err());
        Ok(())
    }

    #[test]
    fn changes_only_display_control_fields_for_each_config_version() -> anyhow::Result<()> {
        for version in 0..=2 {
            let mut config = vec![0xaa; config_size(version)?];
            config[CONFIG_VERSION_OFFSET] = version;
            let offsets = display_offsets(version)?;
            config[BACKLIGHT_DUTY_OFFSET] = 100;
            config[offsets.timeout_mode] = TIMEOUT_MODE_STATIC;
            config[offsets.timeout_seconds] = AWAKE_TIMEOUT_SECONDS;
            let before = config.clone();
            let desired = desired_display_settings(DisplayTarget::Sleep)?;

            set_config_display(&mut config, desired)?;

            assert_eq!(display_settings(&config)?, desired);
            for (offset, (old, new)) in before.iter().zip(&config).enumerate() {
                if [
                    BACKLIGHT_DUTY_OFFSET,
                    offsets.timeout_mode,
                    offsets.timeout_seconds,
                ]
                .contains(&offset)
                {
                    continue;
                }
                assert_eq!(old, new, "config version {version}, byte {offset}");
            }
        }
        Ok(())
    }

    #[test]
    fn awake_and_sleep_settings_are_explicit() -> anyhow::Result<()> {
        assert_eq!(
            desired_display_settings(DisplayTarget::Awake {
                brightness_percent: 100,
            })?,
            DisplaySettings {
                brightness_percent: 100,
                timeout_mode: TIMEOUT_MODE_STATIC,
                timeout_seconds: AWAKE_TIMEOUT_SECONDS,
            }
        );
        assert_eq!(
            desired_display_settings(DisplayTarget::Sleep)?,
            DisplaySettings {
                brightness_percent: 0,
                timeout_mode: TIMEOUT_MODE_SLEEP,
                timeout_seconds: SLEEP_TIMEOUT_SECONDS,
            }
        );
        Ok(())
    }

    #[test]
    fn matches_usb_serial_without_case_sensitivity() {
        let ports = vec![WireViewPort {
            path: "/dev/ttyACM0".into(),
            serial: "20ABCD".into(),
            manufacturer: None,
            product: None,
        }];

        assert_eq!(
            find_port(&ports, "20abcd").map(|port| port.path.as_str()),
            Some("/dev/ttyACM0")
        );
    }

    #[test]
    fn splits_configuration_into_offset_frames() -> anyhow::Result<()> {
        let config = (0_u8..96).collect::<Vec<_>>();

        let frames = config_frames(&config)?;

        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][..2], &[CMD_WRITE_CONFIG, 0]);
        assert_eq!(&frames[0][2..], &config[..62]);
        assert_eq!(&frames[1][..2], &[CMD_WRITE_CONFIG, 62]);
        assert_eq!(&frames[1][2..], &config[62..]);
        Ok(())
    }

    #[test]
    fn validates_the_complete_welcome_message() -> anyhow::Result<()> {
        validate_welcome("wireview", WELCOME_MESSAGE)?;
        assert!(validate_welcome("wireview", b"Thermal Grizzly").is_err());
        assert!(validate_welcome("wireview", &[0xef, 0x05, 0x05]).is_err());
        Ok(())
    }
}
