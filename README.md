# Feather

Feather controls fans, lighting, and display backlights on Linux. It supports Corsair iCUE LINK hardware, NVIDIA GPUs, Linux hwmon sensors, and Thermal Grizzly WireView Pro II displays.

The installation has two executables:

| Executable | Responsibility |
| --- | --- |
| `featherd` | Owns the hardware, evaluates policy, exposes the local socket, and restores fan policies during shutdown. |
| `feather` | Sends commands to `featherd` and performs offline configuration checks. |

`feather` has no HID, hwmon, NVML, or systemd dependency. It communicates through `/run/feather/feather.sock`. Feather does not expose a TCP listener or other remote-control transport.

Fan control can damage hardware when it is configured incorrectly. Start with conservative curves and set `fail_safe_percent` to 100. NVIDIA fan writes switch the selected fans to manual control while `featherd` is running.

## Supported hardware

| Driver | Reads | Writes | Selector |
| --- | --- | --- | --- |
| `corsair-icue-link` | LINK subdevices, hub temperature channels, fan RPM, and endpoint diagnostics | Fan percentage and RGB color | USB vendor/product ID, HID serial, and interface |
| `nvidia-nvml` | GPU name, UUID, temperature, fan count, speed, policy, and limits | Per-fan percentage and default-policy restore | GPU UUID |
| `linux-hwmon` | Linux temperature inputs | None | Physical device path, chip name, and label |
| `wire-view-pro-ii` | USB identity and current display configuration | Volatile display sleep and brightness | USB serial number |

Corsair support covers the iCUE LINK System Hub at USB `1b1c:0c3f`. Other Corsair products may use different endpoints or packet formats.

WireView support covers the Thermal Grizzly WireView Pro II at USB `0483:5740`. Feather communicates with its CDC/ACM serial port at 115200 baud. The implementation uses the device configuration and screen-refresh commands documented by the [wireview-linux](https://github.com/emaspa/wireview-linux) and [wireview-hwmon](https://github.com/emaspa/wireview-hwmon) projects. Feather contains an independent Rust implementation of the required protocol messages.

## NVIDIA implementation

`featherd` calls NVML through [`nvml-wrapper`](https://github.com/rust-nvml/nvml-wrapper). It does not parse `nvidia-smi` output.

NVIDIA describes `nvidia-smi` as a command-line interface built on top of NVML and warns that its output is not guaranteed to remain compatible between releases. Direct NVML calls give Feather typed errors, stable UUID selection, per-fan indices, reported fan limits, and default-policy restoration. See the [NVML command reference](https://docs.nvidia.com/deploy/nvml-api/group__nvmlDeviceCommands.html) and [`nvidia-smi` documentation](https://docs.nvidia.com/deploy/nvidia-smi/index.html).

The daemon loads `libnvidia-ml.so.1` from the installed NVIDIA driver. No NVIDIA SDK is needed at build time.

## Safety behavior

- Configuration parsing rejects unknown fields, invalid percentages, missing references, ambiguous channels, and driver/output mismatches.
- `featherd` checks hardware selectors before it reports ready.
- A missing or stale required sensor sends its fan output to `fail_safe_percent`. RGB outputs turn off.
- Repeated fan-write failures stop the daemon after `daemon.failure_limit` attempts. Shutdown then restores firmware or driver control. RGB and display failures remain degraded and retry without interrupting fan control.
- systemd waits 30 seconds between restarts and stops retrying after three starts within five minutes.
- A Corsair `expected_fan_count` mismatch marks the fan output and daemon as degraded. Feather continues controlling every fan channel the hub enumerates.
- Overrides require a finite lifetime. Normal policy resumes after expiry. A fan override below `minimum_percent` also requires `--allow-below-minimum`.
- Profiles assign every configured output on a hardware device or none of them. The Corsair hub changes hardware mode per device, not per output.
- SIGINT, SIGTERM, config reload, and systemd stop restore fan policies. A scheduled WireView display remains in its last state when the daemon stops, so a sleeping display stays off.
- The socket server permits 32 concurrent clients and closes a request that takes longer than 10 seconds.

## Build x86_64 Linux artifacts on Apple Silicon

The build uses Docker Buildx:

```sh
./scripts/build-amd64.sh
```

The script runs formatting, tests, and Clippy in Linux. It cross-compiles the workspace for `x86_64-unknown-linux-gnu`, runs both executables in an amd64 Debian container, validates the example config, and writes:

```text
dist/x86_64-unknown-linux-gnu/feather
dist/x86_64-unknown-linux-gnu/featherd
dist/x86_64-unknown-linux-gnu/SHA256SUMS
dist/x86_64-unknown-linux-gnu/install-server.sh
dist/x86_64-unknown-linux-gnu/uninstall-server.sh
dist/x86_64-unknown-linux-gnu/packaging/
```

The binaries target glibc and link to `libudev.so.1`. NVIDIA configurations also need `libnvidia-ml.so.1` from the host driver.

## Discover hardware

Run discovery before writing a server configuration. It reads descriptors and sensors without setting fan speeds or lighting:

```sh
sudo featherd --discover
sudo featherd --discover --json
```

After the daemon is configured and running, use the client instead:

```sh
feather devices
feather devices --json
```

Use the reported values in `config.example.toml`:

- Select NVIDIA devices by their `GPU-...` UUID rather than their current index.
- Select a Corsair hub with its `unique_id` and `interface`.
- Select an hwmon input with its physical `device`, `chip`, and `label`.
- Select each WireView by its USB `serial`, not its current `/dev/ttyACM*` number.

## Configure

Start from [config.example.toml](config.example.toml). The configuration has six parts:

1. `daemon` sets polling, stale-reading, socket, saved-state, and failure behavior.
2. `devices` name physical controllers.
3. `sensors` name temperatures from hwmon, NVML, or a Corsair hub channel.
4. `curves`, `color_curves`, and `schedules` define policy data.
5. `outputs` name fan channel groups, RGB strips, or display backlights.
6. `profiles` connect outputs to sensors and curves.

An assignment uses the highest source temperature. Every listed source is required. A stale source puts the output in fail-safe mode.

An empty fan `channels` list selects every reported NVIDIA fan. On a Corsair hub, it selects enumerated subdevices that support speed control, including a fan whose speed reading is unavailable. Feather also keeps available speed channels from unknown device models for compatibility.

A WireView display output uses `brightness_percent`, static timeout mode, and a 30-second timeout interval outside its `off_schedule`. During the schedule, Feather sets brightness to zero and selects sleep mode with a five-second timeout. The screen turns off after five seconds. Feather changes only the running configuration, does not write to WireView NVM, and keeps the selected page.

Set `expected_fan_count` on a Corsair device when the physical fan count is known. `feather status` reports a degraded output when the hub enumerates a different number. `feather debug hid-dump hub` lists each subdevice with its channel, model, device ID, and speed state.

Validate and preview a configuration without contacting the daemon or touching hardware:

```sh
feather config check config.example.toml
feather config preview config.example.toml --profile balanced 45 60 75
```

The daemon keeps its current socket and state-file paths during a live reload. Restart `featherd` to apply a changed `daemon.socket` or `daemon.state_file` value. A reload keeps the active profile when it still exists and activates `default_profile` if that profile was removed.

`feather profile set NAME` changes the running daemon only. Add `--persist` to save the startup profile in `/var/lib/feather/state.json`. `profile list` marks the active and saved startup profiles.

## Install with systemd

Copy the complete artifact directory and the host configuration to the Linux server. From that directory, run:

```sh
sudo ./install-server.sh . ./config.toml "$USER"
```

The installer checks every file listed in `SHA256SUMS` and validates the config before stopping the current service. It backs up the installed files and restores them, the previous enablement state, and the previous running service if the update fails. On success, it installs `feather`, `featherd`, `feather-uninstall`, and the systemd, tmpfiles, sysusers, and udev files. It also adds the named operator to the local socket group. The service receives `CAP_SYS_ADMIN` because NVML requires administrator permission for GPU fan writes. Start a new login session before running `feather` without sudo.

Inspect the service with:

```sh
systemctl status featherd.service
journalctl -u featherd.service -f
feather status
```

Remove the executables and service while keeping the configuration and saved profile:

```sh
sudo feather-uninstall
```

Pass `--purge` to remove `/etc/feather`, `/var/lib/feather`, and the `feather` group as well. The uninstaller leaves the shared system journal intact.

## CLI commands

```sh
# Current policy, sensors, outputs, and overrides
feather status
feather status --json
feather status --watch
feather status --watch 500ms
feather status --check

# Discovery through the running daemon
feather devices

# Live config and profiles
feather config reload
feather profile list
feather profile set quiet
feather profile set performance --persist

# Time-limited manual values
feather override fan case_fans 55 --for 10m
feather override fan case_fans 20 --for 30s --allow-below-minimum
feather override led case_lighting 0 80 0 --for 30m
feather override display wireview0_display off --for 30m
feather override display wireview0_display on --for 10m
feather override clear case_fans
feather override clear --all

# Restore hardware control, or resume daemon control
feather output release gpu0_fans
feather output release --all
feather output resume gpu0_fans

# Parsed Corsair topology and raw endpoint data through featherd
feather debug hid-dump hub

# Shell completion
feather completions zsh > _feather
```

`feather status --check` prints status and exits with code 5 when daemon health is degraded or in fail-safe mode. `--watch` refreshes every two seconds unless an interval is supplied. With `--json`, watch mode emits one compact JSON object per line.

Set `FEATHER_SOCKET` or pass `--socket` when the client uses a non-default local socket.

## Workspace layout

```text
crates/feather-core/  Configuration, curve evaluation, IPC, and status types
crates/featherd/      Hardware drivers, policy engine, socket server, and systemd integration
crates/feather-cli/   Local client and offline config commands; binary name: feather
```

The crate boundary prevents the client from linking hardware drivers. `feather-core` contains the versioned newline-delimited JSON protocol used by both executables.

## Development

Rust 1.96 is pinned in `rust-toolchain.toml`.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --no-deps --all-features
```

The workspace builds on macOS for local development. Linux-only HID, hwmon, NVML, and systemd code is compiled and tested in Docker and the Linux CI job. CI also runs `cargo audit`; tagged releases build and publish the tested amd64 artifact.

## License

MIT. See [LICENSE.md](LICENSE.md).
