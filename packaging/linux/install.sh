#!/bin/sh
# StudyForge installer for Linux (systemd).
#
#   sudo ./install.sh [UID ...]
#
# * installs studyforge + studyd to /usr/local/bin
# * writes /etc/studyd/allowed_users (who may talk to studyd and whose
#   processes it may stop). Defaults to the user who ran sudo/pkexec.
# * installs the systemd unit, desktop entry and icon, enables + starts studyd
#
# Binaries are taken from, in order: this script's directory, ../../target/release.
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "This installer must run as root:  sudo $0" >&2
    exit 1
fi

HERE=$(cd "$(dirname "$0")" && pwd)

# --- who is allowed ---------------------------------------------------------
UIDS="$*"
[ -z "$UIDS" ] && UIDS="${PKEXEC_UID:-${SUDO_UID:-}}"
if [ -z "$UIDS" ] || [ "$UIDS" = "0" ]; then
    echo "Could not tell which user to allow. Run as: sudo $0 \$(id -u)  (from your normal user's shell)" >&2
    exit 1
fi
for u in $UIDS; do
    case "$u" in
        ''|*[!0-9]*) echo "'$u' is not a numeric uid" >&2; exit 1 ;;
    esac
done

# --- find binaries ----------------------------------------------------------
find_bin() {
    # last resort: re-running from /usr/share/studyforge to allow another user
    for d in "$HERE" "$HERE/../../target/release" /usr/local/bin; do
        if [ -x "$d/$1" ]; then echo "$d/$1"; return 0; fi
    done
    return 1
}
STUDYD=$(find_bin studyd) || { echo "studyd not found. Build first:  cargo build --release" >&2; exit 1; }
GUI=$(find_bin studyforge) || { echo "studyforge not found. Build first:  cargo build --release" >&2; exit 1; }

echo "==> Installing binaries to /usr/local/bin"
[ "$STUDYD" = /usr/local/bin/studyd ] || install -Dm755 "$STUDYD" /usr/local/bin/studyd
[ "$GUI" = /usr/local/bin/studyforge ] || install -Dm755 "$GUI" /usr/local/bin/studyforge

echo "==> Writing /etc/studyd/allowed_users"
install -dm755 /etc/studyd
touch /etc/studyd/allowed_users
[ -s /etc/studyd/allowed_users ] || echo "# uids allowed to use studyd (one per line)" > /etc/studyd/allowed_users
for u in $UIDS; do
    grep -qx "$u" /etc/studyd/allowed_users || echo "$u" >> /etc/studyd/allowed_users
done
chmod 644 /etc/studyd/allowed_users

echo "==> Installing desktop entry, icon and the install/uninstall scripts"
install -Dm644 "$HERE/studyforge.desktop" /usr/share/applications/studyforge.desktop
[ -f "$HERE/studyforge.png" ] && install -Dm644 "$HERE/studyforge.png" /usr/share/icons/hicolor/256x256/apps/studyforge.png
if [ "$HERE" != /usr/share/studyforge ]; then
    for f in install.sh uninstall.sh; do install -Dm755 "$HERE/$f" "/usr/share/studyforge/$f"; done
    for f in studyd.service studyforge.desktop studyforge.png; do
        [ -f "$HERE/$f" ] && install -Dm644 "$HERE/$f" "/usr/share/studyforge/$f"
    done
fi
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q /usr/share/icons/hicolor 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q /usr/share/applications 2>/dev/null || true

echo "==> Enabling the systemd service"
install -Dm644 "$HERE/studyd.service" /etc/systemd/system/studyd.service
systemctl daemon-reload
systemctl enable --now studyd.service

echo
echo "StudyForge helper installed and running."
echo "  status:            systemctl status studyd"
echo "  emergency unlock:  sudo studyd emergency-unlock"
