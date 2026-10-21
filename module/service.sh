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
# 与 update-binary 的 select_config 保持同一张平台映射表。
# 未知平台不再 fallback 到 mtd9400（频率表不匹配会导致限频失真），
# 改为写 auto 标记，由 uperf-rs 运行时自动探测集群拓扑与 OPP。
ensure_config() {
    [ -f "$UPERF_CFG" ] && return 0

    PLATFORM="$(getprop ro.board.platform 2>/dev/null)"
    echo "$(date): platform.json missing, detecting platform: $PLATFORM" >> "$LOG_FILE"

    write_auto_config() {
        cat > "$UPERF_CFG" <<'EOF'
{"meta":{"name":"auto","author":"uperf-rs"}}
EOF
        echo "$(date): platform $PLATFORM not mapped, wrote auto config (runtime detect)" >> "$LOG_FILE"
    }

    detect_9400_variant() {
        MAX="$(cat /sys/devices/system/cpu/cpu7/cpufreq/cpuinfo_max_freq 2>/dev/null)"
        [ -n "$MAX" ] && [ "$MAX" -ge 3700000 ] 2>/dev/null && echo "mtd9400p" || echo "mtd9400"
    }

    case "$PLATFORM" in
        # ── 联发科 ──
        mt6983)    CFG="mtd9000"  ;;  # 天玑 9000
        mt6985)    CFG="mtd9200"  ;;  # 天玑 9200
        mt6989)    CFG="mtd9300"  ;;  # 天玑 9300
        mt6991)    CFG="$(detect_9400_variant)" ;;  # 天玑 9400 / 9400+
        mt6993)    CFG="mtd9500"  ;;  # 天玑 9500
        mt6897)    CFG="mtd8300u" ;;
        mt6896)    CFG="mtd8300"  ;;
        # ── 高通 ──
        taro)      CFG="sd8gen1p" ;;
        kalama)    CFG="sd8gen2"  ;;
        pineapple) CFG="sd8gen3"  ;;
        sun)       CFG="sd8elite"  ;;  # 骁龙 8 Elite (SM8750)
        kaanapali) CFG="sd8elite2" ;;  # 骁龙 8 Elite Gen 2 (SM8850)
        cliffs)    CFG="sd8sgen3" ;;   # 骁龙 8s Gen 3 (SM8635)
        cliffs7)   CFG="sd8sgen3" ;;   # 骁龙 7+ Gen 3
        # ── 未知平台 → auto 运行时探测 ──
        *)         write_auto_config; return 0 ;;
    esac

    SRC="$MODDIR/config/${CFG}.json"
    if [ -f "$SRC" ]; then
        cp -f "$SRC" "$UPERF_CFG"
        echo "$(date): selected config: $CFG" >> "$LOG_FILE"
    else
        # 已映射但配置文件缺失（如新增 SoC 未随模块发布）→ auto 兜底
        write_auto_config
    fi
}

# ── 日志轮转：超过 2MB 时保留为 .old，防止 /sdcard 日志无限增长 ───────────────
rotate_log() {
    [ -f "$LOG_FILE" ] || return 0
    SIZE="$(stat -c %s "$LOG_FILE" 2>/dev/null || echo 0)"
    if [ "$SIZE" -gt 2097152 ] 2>/dev/null; then
        mv -f "$LOG_FILE" "$LOG_FILE.old"
        echo "$(date): log rotated (prev size $SIZE bytes)" > "$LOG_FILE"
    fi
}

rotate_log

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
