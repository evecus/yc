#!/system/bin/sh
# init_once.sh — one-shot boot initialisation
# Disables OEM performance boost interfaces that would fight uperf's governor.
# Runs once per boot from service.sh before uperf starts.

write() {
    [ -f "$1" ] && echo "$2" > "$1" 2>/dev/null && echo "  set $1 = $2"
}

echo "=== uperf init_once $(date) ==="

# ── 联发科通用 ────────────────────────────────────────────────────────────────

# 关闭 MTK PPM (Platform Power Manager) 强制锁频
write /proc/ppm/enabled 0

# 关闭 MTK perfmgr boost (used by GameSDK)
write /proc/perfmgr/boost_ctrl/cpu_ctrl/perfmgr_boost 0
write /sys/module/mtk_perfmgr/parameters/boost 0

# 关闭 MTK EAS boost hint (fpsgo / hps)
write /sys/module/mtk_eas_plus/parameters/boost 0
write /proc/cpufreq/cpufreq_cci_mode 0

# 关闭 FPSGO（MTK 帧率辅助，会和我们的 hint 冲突）
write /sys/module/fpsgo/parameters/fpsgo_enable 0
write /sys/kernel/fpsgo/fpsgo_enable 0

# ── 调度器参数 ────────────────────────────────────────────────────────────────

# 关闭 schedboost（留给 uperf 按场景控制）
write /proc/sys/kernel/sched_boost 0

# EAS schedutil rate limit — 放宽上采样，uperf 自己管频率
for cpu in /sys/devices/system/cpu/cpu*/cpufreq/schedutil/rate_limit_us; do
    write "$cpu" 2000
done

# ── cgroup / cpuset 初始值 ────────────────────────────────────────────────────
# 确保 top-app 可以用到所有核心，uperf 会在 idle 时缩减
CPUSET_TOP="/dev/cpuset/top-app/cpus"
[ -f "$CPUSET_TOP" ] && write "$CPUSET_TOP" "0-7"

# Android 13+ cgroup v2 路径
CGROUP2_TOP="/sys/fs/cgroup/top-app/cpuset.cpus"
[ -f "$CGROUP2_TOP" ] && write "$CGROUP2_TOP" "0-7"

# ── GPU DVFS 初始值 ───────────────────────────────────────────────────────────
# 确保 GPU DVFS 开启，uperf 的 GpuGovernor 会接管
write /sys/module/ged/parameters/gpu_dvfs_enable 1
write /sys/module/ged/parameters/ged_boost_enable 0  # 关掉 GED 自带 boost

echo "=== init_once done ==="
