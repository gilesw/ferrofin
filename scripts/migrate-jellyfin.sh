#!/usr/bin/env bash
# Copy a Debian-package Jellyfin installation into Ferrofin's data directory.
# Run after stopping both services and before starting Ferrofin for the first time.
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
    echo "Run this script as root (for example: sudo $0)" >&2
    exit 1
fi

if ! command -v rsync >/dev/null 2>&1; then
    echo "rsync is required; install it with: apt-get install rsync" >&2
    exit 1
fi

source_data=${1:-/var/lib/jellyfin}
source_config=${2:-/etc/jellyfin}
destination=${3:-/var/lib/ferrofin/data}

if [[ ! -d "$source_data" ]]; then
    echo "Jellyfin data directory not found: $source_data" >&2
    exit 1
fi
if [[ ! -f "$source_data/data/jellyfin.db" && ! -f "$source_data/jellyfin.db" ]]; then
    echo "No jellyfin.db found under $source_data" >&2
    exit 1
fi
if ! getent passwd ferrofin >/dev/null; then
    echo "Ferrofin service user does not exist; install the Ferrofin package first" >&2
    exit 1
fi
if [[ -e "$destination/jellyfin.db" || -e "$destination/data/jellyfin.db" ]]; then
    echo "A database already exists under $destination; migrate before the first Ferrofin start" >&2
    exit 1
fi

mkdir -p "$destination" "$destination/config"

# Preserve the directory layout expected by Ferrofin: in particular,
# Jellyfin's root/default libraries land at data/root/default/.
rsync -a --ignore-existing --chown=ferrofin:ferrofin \
    "$source_data/" "$destination/"

# Debian stores Jellyfin's XML configuration separately from its data.
# Merge it into Ferrofin's config directory without replacing existing files.
if [[ -d "$source_config" ]]; then
    rsync -a --ignore-existing --chown=ferrofin:ferrofin \
        "$source_config/" "$destination/config/"
fi

echo "Copied Jellyfin data to $destination"
if [[ -d "$source_config" ]]; then
    echo "Merged Jellyfin configuration from $source_config into $destination/config"
fi
