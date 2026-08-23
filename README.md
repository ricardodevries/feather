<img align="right" src="assets/feather-logo.png" width="180" alt="Feather logo">

# Feather

[![CI](https://github.com/ricardodevries/feather/actions/workflows/ci.yml/badge.svg)](https://github.com/ricardodevries/feather/actions/workflows/ci.yml)
[![Latest tagged release](https://img.shields.io/github/v/release/ricardodevries/feather?display_name=tag&sort=semver)](https://github.com/ricardodevries/feather/releases/latest)

Feather runs thermal policy for Linux systems with Corsair iCUE LINK fans, NVIDIA GPUs, Linux hwmon sensors, and Thermal Grizzly WireView Pro II displays.

`featherd` owns the hardware and restores fan policies during shutdown. `feather` controls the daemon through `/run/feather/feather.sock`; Feather does not open a network port.

## Quick start

Download the Linux amd64 archive from [Releases](https://github.com/ricardodevries/feather/releases), unpack it, and discover the hardware:

```sh
sudo ./featherd --discover
```

Copy and edit the example configuration, then install both executables and the systemd service:

```sh
cp config.example.toml config.toml
./feather config check config.toml
sudo ./install-server.sh . ./config.toml "$USER"
```

Start a new login session so your group membership is refreshed, then check the daemon:

```sh
feather doctor
feather status
```

Building Linux amd64 artifacts on Apple Silicon takes one command:

```sh
./scripts/build-amd64.sh
```

## Common commands

```sh
feather status --watch
feather devices
feather profile set quiet
feather profile set performance --persist

feather override fan case_fans 55 --for 10m
feather override display wireview0_display off --for 30m
feather override clear --all

feather output test front_fans
feather debug hid-dump hub
```

`feather output test` checks each reported fan channel at 35%, 50%, 75%, and 100% duty. It restores normal policy after the test and after a Ctrl+C interruption.

## Supported hardware

| Driver | Hardware | Control |
| --- | --- | --- |
| `corsair-icue-link` | iCUE LINK System Hub `1b1c:0c3f` | Fan duty, RPM, topology, and RGB |
| `nvidia-nvml` | NVIDIA GPUs supported by the installed NVML library | Temperature and per-fan duty |
| `linux-hwmon` | Linux hwmon temperature inputs | Temperature |
| `wire-view-pro-ii` | Thermal Grizzly WireView Pro II `0483:5740` | Volatile brightness and display sleep |

Feather selects Corsair hubs by HID serial, NVIDIA GPUs by UUID, hwmon inputs by physical path and label, and WireViews by USB serial number.

## Safety

Fan control can damage hardware when configured incorrectly. Use conservative curves and set `fail_safe_percent` to 100.

- Missing or stale required sensors put fan outputs at their fail-safe duty.
- Repeated fan-write failures stop the daemon and restore hardware policy.
- Overrides always expire. Values below an output's minimum require explicit confirmation.
- USB operations get one fresh-device retry. Continued fan failures still stop the daemon.
- systemd applies a watchdog, restart limit, filesystem restrictions, and the capability required for NVIDIA fan writes.

## Documentation

- [Hardware and configuration](docs/configuration.md)
- [Installation and operation](docs/operations.md)
- [Development and releases](docs/development.md)

## License

MIT. See [LICENSE.md](LICENSE.md).
