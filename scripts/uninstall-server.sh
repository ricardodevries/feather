#!/bin/sh
set -eu

purge=0
if [ "$#" -eq 1 ] && [ "$1" = "--purge" ]; then
  purge=1
elif [ "$#" -ne 0 ]; then
  echo "usage: $0 [--purge]" >&2
  exit 2
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "run this uninstaller with sudo" >&2
  exit 1
fi

if systemctl is-active --quiet featherd.service; then
  systemctl stop featherd.service
fi
systemctl disable featherd.service >/dev/null 2>&1 || true

if [ -f /usr/lib/tmpfiles.d/feather.conf ]; then
  systemd-tmpfiles --remove /usr/lib/tmpfiles.d/feather.conf || true
fi

rm -f \
  /usr/local/bin/feather \
  /usr/local/bin/featherd \
  /usr/local/sbin/feather-uninstall \
  /etc/systemd/system/featherd.service \
  /usr/lib/sysusers.d/feather.conf \
  /usr/lib/tmpfiles.d/feather.conf \
  /etc/udev/rules.d/99-feather.rules

systemctl daemon-reload
systemctl reset-failed featherd.service >/dev/null 2>&1 || true
udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw
rmdir /run/feather >/dev/null 2>&1 || true

if [ "$purge" -eq 1 ]; then
  rm -rf /etc/feather /var/lib/feather
  if getent group feather >/dev/null 2>&1; then
    groupdel feather
  fi
  echo "Feather, its configuration, and its saved state were removed. The system journal was kept."
else
  echo "Feather was removed. /etc/feather and /var/lib/feather were kept."
fi
