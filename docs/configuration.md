# Hardware and configuration

Start with [`config.example.toml`](../config.example.toml). Feather rejects unknown fields, out-of-range values, missing references, ambiguous hardware selectors, and incompatible driver/output combinations.

## Configuration layout

The file has six sections:

1. `daemon` sets polling, stale-reading, socket, saved-state, driver-timeout, and failure behavior.
2. `devices` names physical controllers.
3. `sensors` names temperature sources.
4. `curves`, `color_curves`, and `schedules` hold policy data.
5. `outputs` names fan groups, RGB strips, and display backlights.
6. `profiles` connect outputs to sensors and curves.

Validate or preview a file without contacting the daemon or touching hardware:

```sh
feather config check config.toml
feather config preview config.toml --profile balanced 45 60 75
```

An assignment uses the highest source temperature. Every listed source is required. A missing or stale source puts a fan output at `fail_safe_percent` and turns an RGB output off.

## Device selectors

Run discovery before writing the configuration:

```sh
sudo featherd --discover
sudo featherd --discover --json
```

Once the daemon is running, discovery is available through the local socket:

```sh
feather devices
feather devices --json
```

Use stable identifiers:

- Select NVIDIA GPUs with the reported `GPU-...` UUID instead of the current index.
- Select a Corsair hub with its `unique_id` and HID `interface`.
- Select hwmon inputs with the physical `device`, chip name, and label.
- Select WireViews with their USB serial numbers instead of `/dev/ttyACM*` paths.

## Corsair iCUE LINK

Feather supports the iCUE LINK System Hub at USB `1b1c:0c3f`. Other Corsair products may use different endpoints or packet formats.

An empty fan `channels` list selects every enumerated subdevice that supports speed control. Feather also keeps available speed channels from unknown device models.

Set `expected_fan_count` when the physical count is known. A mismatch marks the output and daemon as degraded while Feather continues controlling the channels reported by the hub.

```sh
feather debug hid-dump hub
```

The dump lists each subdevice's channel, model, device ID, and speed state. A failed HID operation closes the cached handle, attempts to restore hardware mode, waits 100 milliseconds, and retries once with a fresh handle.

## NVIDIA

Feather calls NVML through [`nvml-wrapper`](https://github.com/rust-nvml/nvml-wrapper). It does not parse `nvidia-smi` output.

Select GPUs by UUID. An empty fan `channels` list selects every reported fan. While `featherd` runs, selected fans use manual control. Shutdown restores the NVIDIA default fan policy.

Each configured GPU is accessed through its own supervised helper process. A blocked NVML call is limited by `daemon.driver_timeout`, which defaults to five seconds. On a timeout or reset-required error, Feather quarantines only that GPU helper, marks its sensor and fan output degraded, and continues controlling non-NVIDIA hardware and other GPUs. Reset the affected GPU, then restart `featherd` to create a fresh helper.

If the helper becomes unresponsive while the GPU uses manual fan control, Feather cannot issue the cleanup call that restores automatic policy. The GPU may retain its last accepted fan target until it is reset or the host is rebooted. Other outputs that depend on its now-stale temperature reading move to their configured fail-safe targets.

NVIDIA's manual fan policy retains the last target. Feather therefore writes a GPU fan target only when the requested percentage changes; `refresh_interval` continues to apply to controllers that require periodic refreshes. Fan count and speed limits are cached, and a successful fan write is reported directly instead of immediately issuing redundant NVML verification reads.

The daemon loads `libnvidia-ml.so.1` from the installed driver. No NVIDIA SDK is needed at build time. NVML fan writes require `CAP_SYS_ADMIN`, which the packaged systemd service grants to `featherd`.

## WireView Pro II

Feather supports the Thermal Grizzly WireView Pro II at USB `0483:5740`. It uses the device's CDC/ACM serial port at 115200 baud and selects each display by USB serial number.

A display output uses `brightness_percent` outside its `off_schedule`. During the schedule, Feather sets brightness to zero and selects sleep mode with a five-second timeout. The display turns off after five seconds.

Feather changes the running configuration and keeps the selected page. It does not write the device's non-volatile memory. A failed serial operation waits 100 milliseconds and retries once with a newly opened port.

The protocol implementation uses messages documented by [wireview-linux](https://github.com/emaspa/wireview-linux) and [wireview-hwmon](https://github.com/emaspa/wireview-hwmon).

## Profiles and reloads

`feather profile set NAME` changes the running daemon. Add `--persist` to save the startup profile in `/var/lib/feather/state.json`.

```sh
feather profile list
feather profile set quiet
feather profile set balanced --persist
feather config reload
```

A reload validates and preflights the new configuration before releasing current hardware. It keeps the active profile when that profile still exists, otherwise it activates `default_profile`.

The daemon keeps its current socket and state-file paths during a reload. Restart `featherd` after changing `daemon.socket` or `daemon.state_file`.

## Failure behavior

- A missing or stale required sensor sends its fan output to `fail_safe_percent`.
- RGB outputs turn off when their sensors are unavailable.
- Repeated fan-write failures stop the daemon after `daemon.failure_limit` attempts, except isolated NVIDIA failures.
- A timed-out or reset-required NVIDIA helper is quarantined; other hardware control continues.
- RGB and display failures remain degraded and retry without stopping fan control.
- Graceful SIGINT, SIGTERM, reload, and systemd stop restore Corsair and responsive NVIDIA fan policy.
- A scheduled WireView remains in its last state when the daemon stops.
- Profiles assign every configured output on a physical device or none of them because a Corsair hub changes control mode per device.
- The socket server permits 32 clients and closes requests that take longer than 10 seconds.
