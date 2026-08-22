#!/bin/sh
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  echo "usage: $0 ARTIFACT_DIRECTORY CONFIG_FILE [OPERATOR_USER]" >&2
  exit 2
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "run this installer with sudo" >&2
  exit 1
fi

artifact_directory=$1
config_file=$2
operator_user=${3:-}
managed_paths='
/usr/local/bin/feather
/usr/local/bin/featherd
/usr/local/sbin/feather-uninstall
/etc/feather/config.toml
/etc/systemd/system/featherd.service
/usr/lib/sysusers.d/feather.conf
/usr/lib/tmpfiles.d/feather.conf
/etc/udev/rules.d/99-feather.rules
'

for command in cp dirname getent grep groupdel id install mkdir mktemp rm sh systemctl systemd-analyze systemd-sysusers systemd-tmpfiles tr udevadm; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "required command is missing: $command" >&2
    exit 1
  fi
done

if [ -n "$operator_user" ]; then
  for command in gpasswd usermod; do
    if ! command -v "$command" >/dev/null 2>&1; then
      echo "required command is missing: $command" >&2
      exit 1
    fi
  done
fi

for path in \
  "$artifact_directory/feather" \
  "$artifact_directory/featherd" \
  "$artifact_directory/SHA256SUMS" \
  "$artifact_directory/install-server.sh" \
  "$artifact_directory/uninstall-server.sh" \
  "$artifact_directory/packaging/systemd/featherd.service" \
  "$artifact_directory/packaging/sysusers/feather.conf" \
  "$artifact_directory/packaging/tmpfiles/feather.conf" \
  "$artifact_directory/packaging/udev/99-feather.rules" \
  "$config_file"
do
  if [ ! -f "$path" ]; then
    echo "required file is missing: $path" >&2
    exit 1
  fi
done

if [ -n "$operator_user" ] && ! id "$operator_user" >/dev/null 2>&1; then
  echo "operator user does not exist: $operator_user" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$artifact_directory" && sha256sum --check SHA256SUMS)
elif command -v shasum >/dev/null 2>&1; then
  (cd "$artifact_directory" && shasum -a 256 --check SHA256SUMS)
else
  echo "sha256sum or shasum is required to verify the artifact" >&2
  exit 1
fi

"$artifact_directory/feather" --version
"$artifact_directory/featherd" --version
"$artifact_directory/feather" config check "$config_file"
sh -n "$artifact_directory/install-server.sh"
sh -n "$artifact_directory/uninstall-server.sh"

backup_directory=$(mktemp -d /var/tmp/feather-install.XXXXXX)
rollback_needed=0
was_active=0
was_enabled=0
operator_added=0
group_existed=0
config_directory_existed=0
state_directory_existed=0
runtime_directory_existed=0

if getent group feather >/dev/null 2>&1; then
  group_existed=1
fi
if [ -d /etc/feather ]; then
  config_directory_existed=1
fi
if [ -d /var/lib/feather ]; then
  state_directory_existed=1
fi
if [ -d /run/feather ]; then
  runtime_directory_existed=1
fi

if systemctl is-active --quiet featherd.service; then
  was_active=1
fi
if systemctl is-enabled --quiet featherd.service; then
  was_enabled=1
fi

for path in $managed_paths; do
  if [ -e "$path" ] || [ -L "$path" ]; then
    backup_path=$backup_directory$path
    mkdir -p "$(dirname "$backup_path")"
    cp -a "$path" "$backup_path"
  fi
done
cp "$config_file" "$backup_directory/new-config.toml"

rollback() {
  echo "Installation failed. Restoring the previous Feather installation." >&2
  set +e
  systemctl stop featherd.service >/dev/null 2>&1
  if [ "$operator_added" -eq 1 ]; then
    gpasswd -d "$operator_user" feather >/dev/null 2>&1
  fi
  for path in $managed_paths; do
    backup_path=$backup_directory$path
    rm -f "$path"
    if [ -e "$backup_path" ] || [ -L "$backup_path" ]; then
      mkdir -p "$(dirname "$path")"
      cp -a "$backup_path" "$path"
    fi
  done
  systemctl daemon-reload >/dev/null 2>&1
  if [ "$was_enabled" -eq 1 ]; then
    systemctl enable featherd.service >/dev/null 2>&1
  else
    systemctl disable featherd.service >/dev/null 2>&1
  fi
  if [ "$was_active" -eq 1 ]; then
    systemctl start featherd.service >/dev/null 2>&1
  fi
  udevadm control --reload-rules >/dev/null 2>&1
  if [ "$config_directory_existed" -eq 0 ]; then
    rmdir /etc/feather >/dev/null 2>&1
  fi
  if [ "$state_directory_existed" -eq 0 ]; then
    rmdir /var/lib/feather >/dev/null 2>&1
  fi
  if [ "$runtime_directory_existed" -eq 0 ]; then
    rmdir /run/feather >/dev/null 2>&1
  fi
  if [ "$group_existed" -eq 0 ]; then
    groupdel feather >/dev/null 2>&1
  fi
}

finish() {
  status=$1
  trap - 0 HUP INT TERM
  if [ "$rollback_needed" -eq 1 ]; then
    rollback
  fi
  rm -rf "$backup_directory"
  exit "$status"
}

trap 'finish $?' 0
trap 'exit 1' HUP INT TERM
rollback_needed=1

if [ "$was_active" -eq 1 ]; then
  systemctl stop featherd.service
fi

install -Dm0644 \
  "$artifact_directory/packaging/sysusers/feather.conf" \
  /usr/lib/sysusers.d/feather.conf
systemd-sysusers

install -Dm0755 "$artifact_directory/feather" /usr/local/bin/feather
install -Dm0755 "$artifact_directory/featherd" /usr/local/bin/featherd
install -Dm0755 "$artifact_directory/uninstall-server.sh" /usr/local/sbin/feather-uninstall

install -d -m0750 -o root -g feather /etc/feather
install -m0640 -o root -g feather "$backup_directory/new-config.toml" /etc/feather/config.toml

install -Dm0644 \
  "$artifact_directory/packaging/tmpfiles/feather.conf" \
  /usr/lib/tmpfiles.d/feather.conf
install -Dm0644 \
  "$artifact_directory/packaging/udev/99-feather.rules" \
  /etc/udev/rules.d/99-feather.rules
install -Dm0644 \
  "$artifact_directory/packaging/systemd/featherd.service" \
  /etc/systemd/system/featherd.service

systemd-tmpfiles --create /usr/lib/tmpfiles.d/feather.conf
udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw
systemd-analyze verify /etc/systemd/system/featherd.service
systemctl daemon-reload

if [ -n "$operator_user" ] && ! id -nG "$operator_user" | tr ' ' '\n' | grep -qx feather; then
  usermod -aG feather "$operator_user"
  operator_added=1
fi

systemctl enable featherd.service
systemctl restart featherd.service
systemctl is-active --quiet featherd.service
/usr/local/bin/feather status

rollback_needed=0
systemctl --no-pager --full status featherd.service
echo "Feather is installed. Start a new login session before using feather without sudo."
