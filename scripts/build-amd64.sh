#!/bin/sh
set -eu

artifact_dir="dist/x86_64-unknown-linux-gnu"
rm -rf "$artifact_dir"
mkdir -p "$artifact_dir"

docker buildx build \
  --target test \
  --progress plain \
  .

docker buildx build \
  --platform linux/amd64 \
  --target smoke \
  --progress plain \
  .

docker buildx build \
  --platform linux/amd64 \
  --target artifact \
  --output "type=local,dest=$artifact_dir" \
  .

set -- \
  feather \
  featherd \
  config.example.toml \
  install-server.sh \
  uninstall-server.sh \
  README.md \
  LICENSE.md \
  assets/feather-logo.png \
  docs/configuration.md \
  docs/operations.md \
  docs/development.md \
  packaging/systemd/featherd.service \
  packaging/sysusers/feather.conf \
  packaging/tmpfiles/feather.conf \
  packaging/udev/99-feather.rules

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$artifact_dir" && sha256sum "$@" > SHA256SUMS)
else
  (cd "$artifact_dir" && shasum -a 256 "$@" > SHA256SUMS)
fi

file "$artifact_dir/feather" "$artifact_dir/featherd"
