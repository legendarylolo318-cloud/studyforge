#!/bin/sh
# Remove StudyForge's helper and clean up everything it may have changed:
# the hosts-file section and browser policies are removed BEFORE the binary is.
#
#   sudo ./uninstall.sh [--purge]    (--purge also deletes /etc/studyd and /var/lib/studyd)
set -u

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root:  sudo $0" >&2
    exit 1
fi

echo "==> Stopping the service"
systemctl disable --now studyd.service 2>/dev/null || true

echo "==> Removing StudyForge hosts entries and browser policies"
if [ -x /usr/local/bin/studyd ]; then
    /usr/local/bin/studyd cleanup || echo "warning: cleanup reported a problem (see above)" >&2
else
    echo "studyd binary missing; removing the marked hosts section by hand"
    sed -i '/# >>> StudyForge BEGIN (do not edit) >>>/,/# <<< StudyForge END <<</d' /etc/hosts
    rm -f /etc/chromium/policies/managed/zz-studyforge.json \
          /etc/opt/chrome/policies/managed/zz-studyforge.json \
          /etc/brave/policies/managed/zz-studyforge.json \
          /etc/opt/edge/policies/managed/zz-studyforge.json
    echo "note: Firefox policy changes (if any) are recorded in /var/lib/studyd/policy_state.json" >&2
fi

echo "==> Removing files"
rm -f /etc/systemd/system/studyd.service
systemctl daemon-reload
rm -f /usr/local/bin/studyd /usr/local/bin/studyforge
rm -f /usr/share/applications/studyforge.desktop
rm -f /usr/share/icons/hicolor/256x256/apps/studyforge.png
rm -rf /usr/share/studyforge
rm -f /run/studyd/studyd.sock

if [ "${1:-}" = "--purge" ]; then
    echo "==> Purging configuration and state"
    rm -rf /etc/studyd /var/lib/studyd
else
    echo "(kept /etc/studyd and /var/lib/studyd; use --purge to delete them)"
fi
echo "Done. Your personal data (~/.local/share/studyforge) was not touched."
