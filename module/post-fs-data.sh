#!/system/bin/sh
# post-fs-data.sh — runs before system mount, very early
# Keep this minimal: only safe to do property reads, no sdcard access

MODDIR="$(dirname "$(readlink -f "$0")")"

# Mark module as active (KernelSU / newer Magisk check this)
touch "$MODDIR/disable" 2>/dev/null && rm "$MODDIR/disable"

# SELinux: nothing to do — uperf only writes sysfs nodes
# that root already has access to
exit 0
