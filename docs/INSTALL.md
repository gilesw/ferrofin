# Installing on bare metal (systemd)

The Docker image and Helm chart in the [README](../README.md#quickstart) bundle everything.
This page is the other path: Ferrofin as a Debian package (or the release tarball) on a
Debian/Ubuntu host under systemd, with jellyfin-ffmpeg and the jellyfin-web client from
Jellyfin's own apt repository.

| Path | What lives there |
|---|---|
| `/usr/bin/ferrofin-server` | the binary (the `.deb` puts it here) |
| `/etc/ferrofin/config.toml` | configuration ([`docs/CONFIG.md`](CONFIG.md)); a conffile, never overwritten on upgrade |
| `/lib/systemd/system/ferrofin.service` | the unit ([`contrib/systemd/ferrofin.service`](../contrib/systemd/ferrofin.service)) |
| `/var/lib/ferrofin/data` | `jellyfin.db`, `cache/` (transcodes), `log/`, `plugins/`, `config/` |
| `/usr/lib/jellyfin-ffmpeg/` | jellyfin-ffmpeg (`ffmpeg`, `ffprobe`), from the `jellyfin-ffmpeg8` package |
| `/usr/share/jellyfin/web/` | jellyfin-web's built client, from the `jellyfin-web` package, served at `/web` |

## 1. Jellyfin's apt repository (ffmpeg and the web client)

Ferrofin does not ship its own ffmpeg. It uses **jellyfin-ffmpeg**, the same build the
release image bundles: SIMD tonemapping, current libx264/libx265, `libfdk_aac`, and
`--enable-chromaprint` for the intro skipper. Your distro's `ffmpeg` works for basic
transcodes but lacks several of those, and Ferrofin plans every transcode against the
binary it probes at startup, so the package depends on `jellyfin-ffmpeg8` and `jellyfin-web`
outright. apt can only satisfy that once the repository is configured:

```sh
sudo apt-get install -y ca-certificates curl gnupg
curl -fsSL https://repo.jellyfin.org/jellyfin_team.gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/jellyfin.gpg
echo "deb [signed-by=/usr/share/keyrings/jellyfin.gpg] https://repo.jellyfin.org/debian $(. /etc/os-release; echo "$VERSION_CODENAME") main" \
  | sudo tee /etc/apt/sources.list.d/jellyfin.list
sudo apt-get update
```

Replace `debian` with `ubuntu` in the repository line on Ubuntu. Do **not** install the
`jellyfin-server` package on the same host unless you mean to run both; they would race for
port 8096.

## 2. Install the package

Download the `.deb` for your architecture from the
[releases page](https://github.com/mangoleaf/ferrofin/releases) and install it; apt pulls
`jellyfin-ffmpeg8` and `jellyfin-web` with it:

```sh
V=1.0.1; A=amd64                                  # or arm64
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/v$V/ferrofin_${V}_$A.deb"
sudo apt-get install -y "./ferrofin_${V}_$A.deb"
```

The package creates the `ferrofin` system user and `/var/lib/ferrofin/data`, installs the
unit, and **does not enable or start it**: the first boot logs the generated admin password
once, and you want to be watching when it does. Give the `ferrofin` user read access to your
media first, typically by adding it to the group that owns the library, then:

```sh
sudo systemctl enable --now ferrofin
journalctl -u ferrofin -f                         # the admin password is logged here once
```

Set `admin_password` in `/etc/ferrofin/config.toml` before the first start for a headless
install. Then open `http://host:8096/web`.

### Without the package: the release tarball

The tarball carries only the binary. Do step 1, install the two packages by hand
(`sudo apt-get install -y jellyfin-ffmpeg8 jellyfin-web`), and with a checkout of this
repository for the `contrib/` files:

```sh
V=v1.0.1; T=x86_64-unknown-linux-gnu             # or aarch64-unknown-linux-gnu
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/$V/ferrofin-$V-$T.tar.gz"
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/$V/ferrofin-$V-$T.tar.gz.sha256"
sha256sum -c "ferrofin-$V-$T.tar.gz.sha256"
tar xzf "ferrofin-$V-$T.tar.gz"
sudo install -m 755 "ferrofin-$V-$T/ferrofin-server" /usr/local/bin/ferrofin-server
sudo useradd --system --home /var/lib/ferrofin --shell /usr/sbin/nologin ferrofin
sudo install -d -o ferrofin -g ferrofin -m 0750 /var/lib/ferrofin /var/lib/ferrofin/data
sudo install -d /etc/ferrofin
sudo install -m 644 contrib/debian/config.toml /etc/ferrofin/config.toml
sudo install -m 644 contrib/systemd/ferrofin.service /etc/systemd/system/ferrofin.service
sudo mkdir -p /etc/systemd/system/ferrofin.service.d
printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/ferrofin-server --config /etc/ferrofin/config.toml\n' \
  | sudo tee /etc/systemd/system/ferrofin.service.d/binary.conf
sudo systemctl daemon-reload
sudo systemctl enable --now ferrofin
```

The drop-in repoints `ExecStart=` at `/usr/local/bin`; everything else in the unit is
shared with the package.

## 3. The unit

The unit runs with `ProtectSystem=strict`: the filesystem is read-only except
`/var/lib/ferrofin/data`. Scans and playback only read media, so that is enough for most
installs. If you enable deleting items or saving metadata/images into the library from the
UI, add that library to `ReadWritePaths=` in a drop-in:

```sh
sudo systemctl edit ferrofin      # opens an override; add:
# [Service]
# ReadWritePaths=/srv/media
```

For VAAPI/QSV hardware transcoding uncomment the `DeviceAllow=` and
`SupplementaryGroups=` lines the same way. The unit names ffmpeg, ffprobe and the web
client explicitly through `FERROFIN_FFMPEG_PATH`, `FERROFIN_FFPROBE_PATH` and
`FERROFIN_WEB_DIR`, because systemd's `PATH` does not include `/usr/lib/jellyfin-ffmpeg`
and discovery would otherwise land on Debian's `/usr/bin/ffmpeg`.

## 4. Migrating a Jellyfin database

Stop Jellyfin, then copy its `data/jellyfin.db` to `/var/lib/ferrofin/data/jellyfin.db`
(Ferrofin also finds it at `data/jellyfin.db` under the data dir, so copying Jellyfin's
whole data directory works too). Ferrofin adopts a **Jellyfin 10.11.8 through 10.11.11**
database on first boot, writing `jellyfin.db.pre-ferrofin` beside it first. The adoption is
one-way; going back to Jellyfin means restoring that copy. [`docs/UPGRADING.md`](UPGRADING.md)
has the full notes.

A database from any other Jellyfin version is refused with a message naming the unexpected
migration ids. Bring it to 10.11.x under Jellyfin first.

## Upgrading

Install the new `.deb` the same way; a running server is restarted by the package, and
`/etc/ferrofin/config.toml` is a conffile, so your edits survive. For a tarball install,
replace the binary in `/usr/local/bin` and `systemctl restart ferrofin`.

Ferrofin's own migrations run on start. A database that a newer version has migrated may
not open under an older one, so back up `jellyfin.db` before a major upgrade. `apt-get
remove` keeps `/etc/ferrofin` and `/var/lib/ferrofin`; `apt-get purge` removes the
configuration and still leaves `/var/lib/ferrofin` (the database and the pre-adoption
Jellyfin backup) for you to delete by hand.
