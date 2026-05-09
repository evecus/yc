#!/system/bin/sh
# service.sh — runs in late_start service context (system fully booted)
# Called by Magisk / KernelSU after boot_completed

MODDIR="$(dirname "$(readlink -f "$0")")"
UPERF_BIN="$MODDIR/bin/uperf"
UPERF_CFG="$MODDIR/config/platform.json"
USER_DIR="/sdcard/Android/yc/uperf"
LOG_FILE="$USER_DIR/uperf.log"
PID_FILE="/dev/uperf.pid"

# ── 等待系统完全启动 ──────────────────────────────────────────────────────────
wait_until_login() {
    # 等 boot_completed 且解锁（sdcard 可写）
    local i=0
    while [ "$i" -lt 60 ]; do
        if [ "$(getprop sys.boot_completed)" = "1" ] && \
           [ -d "/sdcard/Android" ]; then
            return 0
        fi
        sleep 2
        i=$((i + 1))
    done
    return 1
}

# ── 防止重复启动 ──────────────────────────────────────────────────────────────
if [ -f "$PID_FILE" ]; then
    OLD_PID="$(cat $PID_FILE)"
    if [ -d "/proc/$OLD_PID" ]; then
        exit 0   # already running
    fi
    rm -f "$PID_FILE"
fi

# ── 等待登录 ─────────────────────────────────────────────────────────────────
wait_until_login || {
    echo "$(date): timeout waiting for boot" >> "$LOG_FILE"
    exit 1
}

mkdir -p "$USER_DIR"

# ── 一次性初始化（关闭内核/OEM boost，防止干扰） ────────────────────────────
sh "$MODDIR/script/init_once.sh" >> "$LOG_FILE" 2>&1

# ── 写默认模式文件 ────────────────────────────────────────────────────────────
[ -f "$USER_DIR/cur_powermode.txt" ] || echo "balance" > "$USER_DIR/cur_powermode.txt"

# ── 启动 uperf 守护进程 ───────────────────────────────────────────────────────
echo "$(date): starting uperf" >> "$LOG_FILE"

"$UPERF_BIN" "$UPERF_CFG" >> "$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"

echo "$(date): uperf pid=$(cat $PID_FILE)" >> "$LOG_FILE"
