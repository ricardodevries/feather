# Installation and operation

## Build an amd64 release on Apple Silicon

Docker Buildx runs the Linux checks and cross-compiles both executables:

```sh
./scripts/build-amd64.sh
```

The script runs formatting, tests, and Clippy. It executes the resulting amd64 binaries in Debian, validates the example configuration, and writes the release bundle to `dist/x86_64-unknown-linux-gnu/`.

The binaries target glibc and link to `libudev.so.1`. NVIDIA configurations also need `libnvidia-ml.so.1` from the host driver.

## Install with systemd

Copy the complete release bundle and the host configuration to the server. Run the installer from that directory:

```sh
sudo ./install-server.sh . ./config.toml "$USER"
```

The installer verifies `SHA256SUMS`, checks the configuration, and validates the systemd unit before replacing the running installation. If the update fails, it restores the previous files, enablement state, and running service.

The installation provides:

```text
/usr/local/bin/feather
/usr/local/bin/featherd
/usr/local/sbin/feather-uninstall
/etc/feather/config.toml
/etc/systemd/system/featherd.service
```

It also installs the sysusers, tmpfiles, and udev definitions. Start a new login session after installation so membership in the `feather` socket group takes effect.

## Inspect the daemon

```sh
feather doctor
feather doctor --json
feather status
feather status --watch
feather status --check
systemctl status featherd.service
journalctl -u featherd.service -f
```

`feather doctor` checks the socket type and permissions, client/daemon version agreement, daemon health, every sensor and output, active overrides, driver errors, and hardware discovery. It exits with code 5 when a check fails.

`feather status --check` prints status and exits with code 5 when daemon health is degraded or in fail-safe mode. With `--json`, watch mode emits one compact JSON object per line.

## Test a fan output

```sh
feather output test front_fans
```

The default test applies 35%, 50%, 75%, and 100% duty for 15 seconds each. It records every reported channel's RPM, calculates the median at each step, and warns when a channel falls below 75% of that median. It also checks that RPM responds between the first and last duty.

Only outputs with per-channel RPM can be tested. NVIDIA reports fan duty through NVML, not RPM, so the command rejects NVIDIA outputs before changing them.

Customize the test or emit JSON:

```sh
feather output test front_fans --steps 40,70,100 --hold 10s
feather output test front_fans --minimum-relative 70 --json
```

The command uses finite overrides and refuses to replace an existing override. It clears its override after completion or Ctrl+C and verifies that normal policy resumed. If the client is killed, the current step expires after the hold time plus a short cleanup allowance.

The command exits with code 5 when it finds a slow or unresponsive channel. The JSON report still includes every RPM sample and confirms whether normal policy was restored.

## Profiles and temporary overrides

```sh
feather profile list
feather profile set quiet
feather profile set performance --persist

feather override fan case_fans 55 --for 10m
feather override fan case_fans 20 --for 30s --allow-below-minimum
feather override led case_lighting 0 80 0 --for 30m
feather override display wireview0_display off --for 30m
feather override display wireview0_display on --for 10m
feather override clear case_fans
feather override clear --all
```

Return a physical device to firmware policy, or resume daemon control:

```sh
feather output release gpu0_fans
feather output release --all
feather output resume gpu0_fans
```

Outputs on the same physical device are released together.

## Shell completion

```sh
feather completions bash > feather.bash
feather completions fish > feather.fish
feather completions zsh > _feather
```

## Remove Feather

Keep `/etc/feather` and `/var/lib/feather`:

```sh
sudo feather-uninstall
```

Remove the executables, service, configuration, saved state, and `feather` group:

```sh
sudo feather-uninstall --purge
```

The shared system journal remains in both cases.
