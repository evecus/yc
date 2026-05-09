#!/system/bin/sh
# service.sh — runs in late_start service context

MODDIR="$(dirname "$(readlink -f "$0")")"
UPERF_BIN="$MODDIR/bin/uperf"
UPERF_CFG="$MODDIR/config/platform.json"
USER_DIR="/sdcard/Android/yc/uperf"
LOG_FILE="$USER_DIR/uperf.log"
PID_FILE="/dev/uperf.pid"

# ── 等待系统完全启动 ──────────────────────────────────────────────────────────
wait_until_login() {
    local i=0
    while [ "$i" -lt 60 ]; do
        if [ "$(getprop sys.boot_completed)" = "1" ] && [ -d "/sdcard/Android" ]; then
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
        exit 0
    fi
    rm -f "$PID_FILE"
fi

wait_until_login || {
    echo "$(date): timeout waiting for boot" >> "$LOG_FILE"
    exit 1
}

mkdir -p "$USER_DIR"

# ── 确保 platform.json 存在（安装时未匹配时的运行时补救） ─────────────────────
ensure_config() {
    [ -f "$UPERF_CFG" ] && return 0

    PLATFORM="$(getprop ro.board.platform 2>/dev/null)"
    echo "$(date): platform.json missing, detecting platform: $PLATFORM" >> "$LOG_FILE"

    detect_9400_variant() {
        MAX="$(cat /sys/devices/system/cpu/cpu7/cpufreq/cpuinfo_max_freq 2>/dev/null)"
        [ -n "$MAX" ] && [ "$MAX" -ge 3700000 ] 2>/dev/null && echo "mtd9400p" || echo "mtd9400"
    }

    case "$PLATFORM" in
        mt6897) CFG="mtd8300u" ;;
        mt6896) CFG="mtd8300"  ;;
        mt6991) CFG="$(detect_9400_variant)" ;;
        *)      CFG="mtd9400"
                echo "$(date): WARNING: unknown platform $PLATFORM, using $CFG" >> "$LOG_FILE" ;;
    esac

    SRC="$MODDIR/config/${CFG}.json"
    if [ -f "$SRC" ]; then
        cp -f "$SRC" "$UPERF_CFG"
        echo "$(date): selected config: $CFG" >> "$LOG_FILE"
    else
        echo "$(date): ERROR: config $SRC not found" >> "$LOG_FILE"
        return 1
    fi
}

ensure_config || exit 1

# ── 修复二进制权限（模块目录可能挂为 noexec） ─────────────────────────────────
chmod 0755 "$UPERF_BIN" 2>/dev/null
if [ ! -x "$UPERF_BIN" ]; then
    TMPBIN="/dev/uperf_bin"
    cp -f "$UPERF_BIN" "$TMPBIN" && chmod 0755 "$TMPBIN" && UPERF_BIN="$TMPBIN"
fi

if [ ! -x "$UPERF_BIN" ]; then
    echo "$(date): ERROR: cannot execute $UPERF_BIN" >> "$LOG_FILE"
    exit 1
fi

# ── 一次性初始化 ──────────────────────────────────────────────────────────────
sh "$MODDIR/script/init_once.sh" >> "$LOG_FILE" 2>&1

# ── 默认模式文件 ──────────────────────────────────────────────────────────────
[ -f "$USER_DIR/cur_powermode.txt" ] || echo "balance" > "$USER_DIR/cur_powermode.txt"

# ── 启动守护进程 ──────────────────────────────────────────────────────────────
echo "$(date): starting uperf (bin=$UPERF_BIN cfg=$UPERF_CFG)" >> "$LOG_FILE"

"$UPERF_BIN" "$UPERF_CFG" >> "$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"

echo "$(date): uperf pid=$(cat $PID_FILE)" >> "$LOG_FILE"
