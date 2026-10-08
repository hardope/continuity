#!/bin/bash
# System packages continuityd needs to build (and test) on Linux — shared by
# the Linux build and test jobs in .github/workflows/release.yml.
set -euo pipefail

sudo apt-get update
# - libayatana-appindicator3-dev: the tray icon. libappindicator3-dev
#   doesn't exist on Ubuntu 24.04+ any more; this is its replacement.
# - libxdo-dev: muda (tray-icon's menu library) links against libxdo for
#   keyboard accelerators, even though none are set yet.
# - dpkg-dev: provides dpkg-shlibdeps, which cargo-deb's "$auto" Depends:
#   resolution (core/continuityd/Cargo.toml) needs — without it cargo-deb
#   silently produces a .deb with no dependency information at all.
# - libpipewire-0.3-dev / libspa-0.2-dev / libclang-dev: remote control's
#   screen capture (the pipewire/libspa-sys crates link against them via
#   pkg-config, and run bindgen, which needs libclang, over their headers).
# - libgtk-3-dev / libdbus-1-dev / pkg-config: the tray and dialogs (GTK),
#   the keyring's Secret Service backend (D-Bus).
sudo apt-get install -y \
  libgtk-3-dev libayatana-appindicator3-dev \
  libdbus-1-dev libxdo-dev pkg-config dpkg-dev \
  libpipewire-0.3-dev libspa-0.2-dev libclang-dev
