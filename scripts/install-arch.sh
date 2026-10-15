#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
build_dir="${repo_dir}/aur/local"

case "${1:-}" in
  '') install_package=1 ;;
  --prepare) install_package=0 ;;
  *) printf 'Usage: %s [--prepare]\n' "$0" >&2; exit 2 ;;
esac
if (( $# > 1 )); then
  printf 'Usage: %s [--prepare]\n' "$0" >&2
  exit 2
fi

for command in makepkg tar sha256sum sed cargo; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'Missing required command: %s\n' "$command" >&2
    exit 1
  fi
done
if (( EUID == 0 )); then
  printf 'Run this script as a regular user; makepkg will request sudo for installation.\n' >&2
  exit 1
fi

version="$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "${repo_dir}/Cargo.toml" | head -n 1)"
if [[ -z "$version" ]]; then
  printf 'Could not read the package version from Cargo.toml.\n' >&2
  exit 1
fi
if [[ "$(sed -n 's/^pkgver=//p' "${repo_dir}/aur/PKGBUILD.local")" != "$version" ]]; then
  printf 'Update pkgver in aur/PKGBUILD.local to %s first.\n' "$version" >&2
  exit 1
fi

mkdir -p "$build_dir"
archive="stitchviewer-${version}.tar.gz"
files=(Cargo.toml Cargo.lock README.md LICENSE src/*.rs assets/stitchviewer.svg assets/stitchviewer.png aur/stitchviewer.desktop)
(
  cd "$repo_dir"
  tar -czf "${build_dir}/${archive}" \
    --transform="s|^|stitchviewer-${version}/|" "${files[@]}"
)
checksum="$(sha256sum "${build_dir}/${archive}")"
checksum="${checksum%% *}"
sed "s/^sha256sums=('SKIP')$/sha256sums=('${checksum}')/" \
  "${repo_dir}/aur/PKGBUILD.local" > "${build_dir}/PKGBUILD"
(
  cd "$build_dir"
  makepkg --printsrcinfo > .SRCINFO
)

if (( install_package )); then
  cd "$build_dir"
  makepkg -fsi
else
  printf 'Prepared %s\n' "$build_dir"
  printf 'Build and install with: yay -Bi --rebuild %s\n' "$build_dir"
fi
