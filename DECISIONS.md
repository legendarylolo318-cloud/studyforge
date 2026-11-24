# Design decisions, deviations and known limitations

## Deviations from the suggested stack

| Choice | Why |
|---|---|
| **`rfd`** added (file/folder dialogs) | CSV / `.apkg` import and the data-folder chooser need native pickers; not on the original list. Uses the XDG portal on Linux. |
| **`tokio`** only on Linux, only for `ashpd` (GlobalShortcuts portal) and `zbus` (KDE DND) | Both are async-only (zbus 6 dropped its blocking API). Each use is a tiny current-thread `block_on`; the app does not otherwise run an async runtime. |
| **`gtk`** dependency on Linux | `tray-icon` needs a GTK main loop on the thread that owns the tray. The tray therefore runs on its own `tray-gtk` thread; Slint keeps winit. |
| **Slint `fluent` style** + own `Theme` global | Slint's std widgets follow `Palette.color-scheme`; `AppWindow` keeps it in sync with the app theme (dark/light/system). |
| `studyd` is a lib + bin (`studyd_lib`) | So the end-to-end test can run the real daemon wiring in-process. |
| `rusqlite_migration` migrations are plain SQL strings | Simple, versioned via `PRAGMA user_version`; validated in a unit test. |

## Timer
* Countdowns use the monotonic clock (`Instant`). On Linux `CLOCK_MONOTONIC` does not advance while suspended, so each tick also looks at the wall clock: if wall time moved forward by more than 5 s beyond the monotonic delta the gap is credited ("recompute from start + elapsed"). Wall-clock jumps *backwards* are ignored.
* A long suspend can finish a phase but never chains several phases and never over-credits focus time (credit is capped at the phase length; flow mode credits at most 10 min of any gap).
* Skipping a work phase does not count as a completed cycle.

## Streaks
Replay of daily totals: a day counts at ≥ the threshold; every 7th consecutive day banks one freeze (max 2); a missed day *before today* consumes a freeze if available (streak neither grows nor resets), otherwise resets. Today never breaks a streak. A session counts toward the study day it **started** in (day starts at a configurable local hour, default 04:00).

## Spaced repetition
SM-2 style with four buttons: Again → relearn (due today, re-queued a few cards later, ease −0.20, lapse counted only for review cards); Hard ×1.2 (ease −0.15); Good 1 d → 6 d → ×ease; Easy 4 d → 8 d → ×ease×1.3 (ease +0.15); ease floor 1.3; Good/Easy always grow the interval. Everything is behind `trait Scheduler` for a future FSRS.

## Markdown preview
Slint cannot lay out mixed bold/plain runs inside one wrapped `Text`. Blocks (headings, paragraphs, lists, code, quotes, rules) are real elements; **bold/italic is applied when a whole block is emphasised**, otherwise the markers are dropped and the text kept. Links are shown as `text (url)`.

## Import
* CSV: delimiter sniffing (`, ; \t`), BOM stripped, column-mapping preview, 32 MB cap.
* Anki: prefers `collection.anki21b` (zstd) → `.anki21` → `.anki2`; first two fields → front/back; cloze notes become `[...]` / revealed text; HTML → plain text with line breaks. **All notes go into one deck** (deck structure is not imported). Media is skipped and counted. Only known collection names are ever extracted (zip-slip is impossible by construction; unsafe names are ignored); extraction is capped while copying (also protects against zstd bombs); the legacy "please update Anki" placeholder package is rejected with an explanation.

## Blocking
* **Domains only.** IP addresses are rejected at every layer (UI input, spec building, hosts rendering).
* **Hosts file**: marker-delimited section, `0.0.0.0` + `::` per domain, atomic replace preserving line endings *and file mode* (a clippy lint caught an early `set_readonly(false)` that would have made `/etc/hosts` world-writable on Unix; there is now a regression test), symlinks resolved, bind-mount fallback (in-place write), Windows read-only attribute and sharing-violation retries, backup once. Corrupt sections (BEGIN without END) are repaired conservatively (only our own entry lines are removed).
* **Firefox policies** are merged with the user's `policies.json`; a bookkeeping file in the state dir records exactly what was added (including the previous `DNSOverHTTPS` value) so removal restores the user's file and deletes it only if we created it. Targets: `/etc/firefox/policies/policies.json` plus each existing `<install>/distribution/`. Flatpak/Snap Firefox is **not** covered (sandboxed paths).
* **Chromium family**: one dedicated managed file `zz-studyforge.json` per policy dir (Chrome, Chromium, Brave, Edge). Other policy files are never edited; their `URLBlocklist`/`URLAllowlist` entries are folded into ours so they keep working if the browser lets a later file win. *Not verified against every browser build.* On Windows the same logic runs against the registry through a `Registry` trait (tested with a fake; the `winreg` backend is Windows-only).
* **Allowlist mode**: websites via policies only (`URLBlocklist *` + `URLAllowlist`, Firefox `<all_urls>` + Exceptions); the UI warns when policies are off. Apps: only processes of the selected lists (the user's curated "distraction category") that are not allowlisted are touched. The "has a visible window" test is **not** implemented; the curated list is the criterion.
* **Distraction log** records app attempts only. Hosts-file blocking is silent, so *website* attempts cannot be observed and are not logged.
* **Process safety**: hard-coded protected names (init/systemd, compositors, shells, terminals, Explorer & friends, studyd, studyforge), owner check against `allowed_users` (never root/SYSTEM), identity re-verified (pid + start time) right before a kill, SIGTERM first then SIGKILL on the next scan. Steam client processes are only touched with the explicit option.
* **Steam detection** follows the spec (env vars, `steamapps/common` of every library from `libraryfolders.vdf`, Flatpak paths, Proton). On Windows the SteamPath is read from `HKEY_USERS\<SID>` (the service runs as SYSTEM, so `HKCU` would be the wrong hive).
* Blocks apply for the **whole session including breaks**.
* Quitting the GUI ends a normal session's block; if the GUI crashes, a normal session lapses after 10 minutes without contact. A hard-mode lock survives both.

## Hard mode
* The deadline is wall-clock UTC and persisted; on start the daemon re-applies a live lock and ends an expired one. Locks are clamped to 24 h server-side and the emergency cooldown floor (10 min) is enforced by the daemon, not just the UI.
* Early exit: typed challenge (48 random chars, rotated after each wrong attempt; paste is blocked via key interception *and* a paste-size heuristic — middle-click paste on X11 is caught by the heuristic only) **or** a cooldown. The emergency unlock always works after its cooldown even when the challenge is disabled.
* Escape hatch: `studyd emergency-unlock` (root/admin) writes a flag file the running daemon honours, clears the persisted session and schedules, and removes hosts/policy sections itself, so it works with the daemon running, stopped, or wedged.
* `systemctl stop studyd` does **not** end a lock (state is persisted and re-applied on the next start) but, as for any root user, nothing can stop root from editing `/etc/hosts` — this is a commitment device, not a security boundary (see README).

## IPC
JSON lines; first message must be `Hello` with the protocol version. Linux: `/run/studyd/studyd.sock` (mode 0666) with `SO_PEERCRED` checked via `interprocess::peer_creds` against `/etc/studyd/allowed_users` (root always allowed) before any byte is read; max 16 connections, 1 MiB lines. Windows: named pipe with a DACL for SYSTEM + Administrators + the installing user's SID (SIDs are validated before going into the SDDL string).

## Platform notes
* **Wayland**: the overlay requests `always-on-top`, which plain `xdg_toplevel`s cannot force; on Hyprland/Sway add a window rule (e.g. Hyprland `windowrulev2 = float, title:^(StudyForge Focus Check)$` and `windowrulev2 = pin, title:^(StudyForge Focus Check)$`). The global hotkey uses the GlobalShortcuts portal; if absent the feature is disabled with a message in Settings and the tray menu remains.
* **GNOME** has no native tray: install the AppIndicator extension.
* **Windows service** runs in session 0 and cannot show UI, so the overlay is GUI-only; the desktop-notification fallback for gentle mode exists on Linux only.
* **DND**: GNOME (`gsettings`, previous value restored and persisted for crash recovery) and KDE (D-Bus `Inhibit`, tied to our bus connection so it vanishes on a crash). Everything else, including Windows, is reported as unsupported.

## Folder sync
Only a lock file (`hostname + pid`, refreshed every 30 s; stale after 2 min) so two computers cannot open the same database at once, plus "take over" for stale locks from another computer. SQLite uses WAL; the app checkpoints at exit so the synced folder holds one consistent file. **There is no merge**: use one computer at a time.

## Not done / future work
* FSRS scheduler (the trait is ready), Anki media import, deck-structure import, FTS search for notes (substring search for now).
* Real merge sync, encryption at rest, translations, accessibility pass.
* Per-monitor overlay placement; website-attempt logging (would need DNS/proxy interception).
* Windows Focus Assist integration (no public API).

## Verification status
Linux: all unit tests, the daemon end-to-end test (real socket, process kill, sandboxed hosts/policy files), `clippy -D warnings`, `rustfmt`. UI layout verified through a headless software-renderer example. **Windows-only code could not be compiled or run during development** (no Windows target available): the service entry points, registry backend, named-pipe ACL, tray-on-main-thread and `explorer`/PowerShell helpers were reviewed against the dependency sources only; CI on `windows-latest` is the first real check.
