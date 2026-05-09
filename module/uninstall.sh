#!/system/bin/sh
# uninstall.sh — called by Magisk/KernelSU on module removal

# Kill running daemon
PID_FILE="/dev/uperf.pid"
if [ -f "$PID_FILE" ]; then
    kill "$(cat $PID_FILE)" 2>/dev/null
    rm -f "$PID_FILE"
fi

# Leave user config intact (/sdcard/Android/yc/uperf/) — don't delete user data
echo "uperf uninstalled. User config kept at /sdcard/Android/yc/uperf/"
