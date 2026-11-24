# StudyForge

A desktop study app written in Rust with the [Slint](https://slint.dev) UI toolkit:
focus timers, flashcards with spaced repetition, notes, tasks, statistics — and an
optional privileged helper (`studyd`) that blocks distracting websites, apps and
Steam games while you study.

Runs on Linux (KDE, GNOME, wlroots/Hyprland best-effort; X11 and Wayland) and Windows 10/11.
Everything except blocking works **without** the helper.

> Screenshots: _placeholder — drop images into `docs/screenshots/` and link them here._
> (`cargo run -p studyforge --example screenshot -- docs/screenshots` renders every page headlessly.)

## Features

**Focus** — Pomodoro (configurable lengths, long break every N cycles, auto-start) and Flow (count-up with optional break reminder); session goals with a Done / Partly / Not done wrap-up; subjects with colours; rotating break tips (editable); ambient sound mixer (procedural white/pink/brown noise + your own `.ogg/.mp3/.wav/.flac` files from `<data dir>/sounds/`); desktop notifications + chime; tray icon with remaining time; global hotkey (default `Ctrl+Alt+S`); Do Not Disturb on GNOME and KDE; start at login.

**Study tools** — Flashcards (SM-2 behind a `Scheduler` trait, new-cards-per-day limit, due counts), CSV import with column-mapping preview, Anki `.apkg` import (zstd `.anki21b`, `.anki21`, `.anki2`; HTML stripped; media skipped and counted; zip-slip / zip-bomb guarded); markdown notes with live preview, autosave and search; tasks with subject, due date and priority.

**Stats & motivation** — streaks with freeze tokens and a configurable day start, daily/weekly goals with progress rings, time by subject, distraction log (by hour and by app).

**Blocking (needs `studyd`)** — app/process blocker, Steam game blocker, domain-based website blocker (hosts file + browser policies), presets (Social Media, Video, Gaming, News) and custom lists, allowlist mode, scheduled blocking (also with the GUI closed), gentle mode (always-on-top "Close it? / 5 more minutes / Dismiss" overlay), and hard mode with an early-exit challenge or cooldown plus an always-working emergency unlock.

**Data** — local SQLite (WAL, foreign keys, versioned migrations), daily backup (last 7 kept), optional custom data folder (e.g. Syncthing/Dropbox) guarded by a lock file.

## Architecture

```
crates/core      lib   models, DB + migrations, timer state machine, SRS, stats/streaks,
                       schedules, hard-mode rules, IPC types, presets, importers. No UI/OS code.
crates/blocker   lib   SiteBlocker / AppBlocker traits + Linux & Windows implementations.
                       Every path it touches is injectable; tests use temp dirs only.
crates/daemon    bin   studyd: privileged helper (systemd service / Windows service).
app/             bin   studyforge: Slint GUI + tray; talks to studyd over a local socket.
```

The GUI never does DB/IPC/file work on the UI thread: a backend thread owns the database and timer
and pushes updates with `slint::invoke_from_event_loop`; a second thread polls `studyd`.

## Build

Needs a recent stable Rust (the Slint 1.19 toolchain requirement applies) and system libraries for
GTK3 (tray), AppIndicator, ALSA, fontconfig and xkbcommon.

**Arch**
```bash
sudo pacman -S --needed base-devel rust pkgconf gtk3 libayatana-appindicator alsa-lib fontconfig libxkbcommon wayland libx11 xdotool
# runtime extras: xdg-desktop-portal + your DE's portal (file dialogs, Wayland hotkey), libnotify
```
**Debian / Ubuntu**
```bash
sudo apt install build-essential pkg-config libgtk-3-dev libayatana-appindicator3-dev libasound2-dev \
    libfontconfig1-dev libxkbcommon-dev libwayland-dev libx11-dev libxdo-dev
# runtime extras: xdg-desktop-portal, libnotify-bin
```
**Fedora**
```bash
sudo dnf install gcc pkgconf-pkg-config gtk3-devel libayatana-appindicator-gtk3-devel alsa-lib-devel \
    fontconfig-devel libxkbcommon-devel wayland-devel libX11-devel libxdo-devel
# runtime extras: xdg-desktop-portal, libnotify
```
**Windows 10/11** — install Rust with the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`)
and the "Desktop development with C++" build tools. No other libraries are required.

```bash
cargo build --release          # target/release/studyforge and target/release/studyd
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

> **Tray note (Linux):** `tray-icon` needs GTK3 and `libayatana-appindicator`. KDE and most wlroots bars
> (waybar) show it; stock GNOME needs the *AppIndicator and KStatusNotifierItem Support* extension.
> If no tray host is running (StudyForge checks for one), the app still works and closing the window quits
> instead of hiding it.

## Install the helper

**Linux (systemd)** — from the source tree after `cargo build --release`:
```bash
sudo ./packaging/linux/install.sh        # allows the user who ran sudo
# or: sudo ./packaging/linux/install.sh 1000 1001   (explicit uids)
```
This installs `studyd` and `studyforge` to `/usr/local/bin`, writes `/etc/studyd/allowed_users`, installs the
unit, desktop entry and icon, and starts `studyd`. The **Install…** button on the Blocking page runs the same
script through `pkexec`. Remove everything again (including the hosts/policy sections) with
`sudo ./packaging/linux/uninstall.sh [--purge]`.

**Windows** — from an *administrator* terminal in the folder containing the binaries:
```powershell
.\studyd.exe install      # registers + starts the "studyd" service; only your SID (and SYSTEM) may connect
.\studyd.exe uninstall    # stops/removes the service and undoes hosts/policy changes
```

**Run without installing the helper:** `cargo run -p studyforge`. The Blocking page then shows
"Blocking helper not installed" and everything else keeps working.

## Using blocking

1. *Blocking* page: tick the lists to use (presets are read-only: duplicate to customise), set each list to
   **hard** (close the app) or **gentle** (ask me).
2. *Focus* page: pick lists, optionally enable **Hard mode** and a lock duration, start the session.
3. *Scheduled blocking* runs inside `studyd`, e.g. "Social Media, Mon–Fri 09:00–17:00" (ranges may cross midnight).
4. *Allowlist mode* blocks everything except what you allow (websites need browser policies).

### Hard mode and the emergency unlock
A hard session cannot be stopped from the GUI; `studyd` owns the deadline (a wall-clock UTC time persisted to
disk, so a reboot does not end it). Ways out: the deadline; the early-exit challenge (type a long random text,
no pasting) or a cooldown (default 20 min); and the **emergency unlock**, which *always* works after its
cooldown (default 60 min, never below 10). On the machine itself an administrator can always run

```bash
sudo studyd emergency-unlock        # Windows: studyd.exe emergency-unlock (admin terminal)
```

which ends the lock, removes schedules and restores the hosts file and browser policies — with the daemon
running, stopped or wedged. Changing the system clock does not shorten or extend a lock: studyd detects clock
jumps and keeps the remaining time; time spent asleep counts.

## How blocking works — and its limits

* **Websites, layer 1 — hosts file.** Domains (never IP addresses) are mapped to `0.0.0.0` and `::` between
  `# >>> StudyForge BEGIN (do not edit) >>>` and `# <<< StudyForge END <<<`. Nothing outside the markers is touched;
  writes are atomic; the original is backed up once to the daemon state dir; DNS caches are flushed.
  Hosts files have no wildcards, so presets list common subdomains (`www.`, `m.`, `mobile.`, …); add more in a custom list.
* **Websites, layer 2 — browser policies** (Settings, on by default). Browsers with DNS-over-HTTPS bypass the
  hosts file, so while a block is active StudyForge also pushes URL-block policies and switches DoH off for Firefox
  (`policies.json`) and Chrome/Chromium/Brave/Edge (managed policy files, or the registry on Windows). Only
  keys/files StudyForge created are removed. Firefox may need a restart to pick up changes.
* **Apps and Steam games** are matched by process name (case-insensitive, `.exe` optional) and by Steam's launch
  environment / `steamapps/common` paths. Protected system processes, anything owned by root/SYSTEM, and processes of
  other users are never touched; the Steam client itself is only closed if you opt in.
* **Limits.** Portable or Flatpak/Snap browsers, other devices, VPNs/proxies that tunnel DNS, and apps launched
  under a different name are not covered. Anyone with administrator rights can undo everything (`studyd` is a
  commitment device, not a security boundary). Website attempts are not logged (only app attempts) because
  hosts-file blocking is silent. Details: [DECISIONS.md](DECISIONS.md).

## Data locations

| What | Linux | Windows |
|---|---|---|
| Database, sounds, backups | `~/.local/share/studyforge/` (changeable in Settings) | `%APPDATA%\StudyForge\data` |
| GUI log | `~/.local/share/studyforge/logs/` | `%LOCALAPPDATA%\StudyForge\data\logs` |
| Helper state / allowed users | `/var/lib/studyd`, `/etc/studyd/allowed_users` | `%ProgramData%\StudyForge` |
| Helper socket | `/run/studyd/studyd.sock` | `\\.\pipe\studyd` |

Set `STUDYFORGE_DATA_DIR` to run with a different data folder (portable use, testing).

## Folder sync (minimal)

*Settings → Data folder* lets you point StudyForge at e.g. a Syncthing/Dropbox folder. A lock file containing
hostname + pid stops two computers/instances opening the database at once and offers a "take over" when the lock
is stale. **This is not merge sync** — use one computer at a time; real merging is future work.

## Developing

```bash
# run the helper without root, touching nothing outside a scratch directory:
mkdir /tmp/sf && cargo run -p studyd -- --sandbox /tmp/sf run
STUDYFORGE_SOCKET=/tmp/sf/studyd.sock cargo run -p studyforge
cargo run -p studyd -- --sandbox /tmp/sf --dry-run run      # log what it would do, change nothing
cargo run -p studyforge --example screenshot -- /tmp/shots  # headless UI render of every page
```
All system-modifying code paths honour `--dry-run`, and the tests only ever write to temp directories.
