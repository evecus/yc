#!/usr/bin/env python3
"""Generate Snapdragon configs from the mtd9400 template.

Roles: little / mid (one or more clusters) / prime.  Template preset
frequencies are rescaled per role by (new_max / template_max).
"""
import json, copy, re

T = json.load(open("config/mtd9400.json"))
# template cluster maxima (kHz) per role: c0=little, c1=mid, c2=prime
TMAX = {"little": 2000000, "mid": 3330000, "prime": 3630000}
TROLE = {"c0": "little", "c1": "mid", "c2": "prime"}

def rng(cpus):
    return f"{cpus[0]}-{cpus[-1]}" if len(cpus) > 1 else str(cpus[0])

PLATFORMS = {
  "sd8gen1p": dict(
    name="sd8gen1p[SM8475 taro]",
    clusters=[  # role, cpus, max_khz, power model (eff, typP, typF, sweet, plain, free), comment
      ("little", [0,1,2,3], 2016000, (100, 0.30, 1.2, 1.6, 1.9, 0.80), "4x Cortex-A510"),
      ("mid",    [4,5,6],   2745600, (300, 0.80, 1.8, 2.4, 2.6, 1.00), "3x Cortex-A710"),
      ("prime",  [7],       3187200, (420, 1.50, 2.0, 2.7, 3.0, 1.10), "1x Cortex-X2"),
    ],
    gpu=(220000, 500000, 900000),            # Adreno 730
    cpuset=dict(Fg="0-6", Bg="0-3", SysBg="0-3", Re="0-6", idleTa="0-6"),
  ),
  "sd8gen2": dict(
    name="sd8gen2[SM8550 kalama]",
    clusters=[
      ("little", [0,1,2], 2016000, (100, 0.28, 1.2, 1.5, 1.8, 0.80), "3x Cortex-A510"),
      ("mid",    [3,4],   2803200, (310, 0.80, 1.8, 2.4, 2.6, 1.00), "2x Cortex-A715"),
      ("mid",    [5,6],   2803200, (290, 0.75, 1.8, 2.4, 2.6, 1.00), "2x Cortex-A710"),
      ("prime",  [7],     3187200, (460, 1.60, 2.0, 2.8, 3.0, 1.10), "1x Cortex-X3"),
    ],
    gpu=(220000, 500000, 719000),            # Adreno 740
    cpuset=dict(Fg="0-6", Bg="0-2", SysBg="0-2", Re="0-6", idleTa="0-6"),
  ),
  "sd8gen3": dict(
    name="sd8gen3[SM8650 pineapple]",
    clusters=[
      ("little", [0,1],   2265600, (100, 0.25, 1.2, 1.6, 2.0, 0.80), "2x Cortex-A520"),
      ("mid",    [2,3,4], 3148800, (330, 0.85, 1.8, 2.5, 2.9, 1.00), "3x Cortex-A720"),
      ("mid",    [5,6],   2956800, (320, 0.80, 1.8, 2.5, 2.8, 1.00), "2x Cortex-A720"),
      ("prime",  [7],     3302400, (500, 1.70, 2.0, 2.8, 3.1, 1.10), "1x Cortex-X4"),
    ],
    gpu=(220000, 520000, 1000000),           # Adreno 750
    cpuset=dict(Fg="0-6", Bg="0-2", SysBg="0-2", Re="0-6", idleTa="0-6"),
  ),
}

def build(key, p):
    c = copy.deepcopy(T)
    cl = p["clusters"]
    n = len(cl)
    c["meta"]["name"] = p["name"]
    c["meta"]["author"] = "community"

    # --- power model ---------------------------------------------------
    c["modules"]["cpu"]["powerModel"] = [
        {"_comment": f"c{i} — {desc}, up to {mx/1e6:.2f} GHz",
         "efficiency": e, "nr": len(cpus), "typicalPower": tp, "typicalFreq": tf,
         "sweetFreq": sw, "plainFreq": pl, "freeFreq": fr}
        for i, (role, cpus, mx, (e,tp,tf,sw,pl,fr), desc) in enumerate(cl)]

    # --- GPU (Adreno / kgsl devfreq) ------------------------------------
    gmin, gmid, gmax = p["gpu"]
    c["modules"]["gpu"] = {"enable": True, "minFreqKhz": gmin, "midFreqKhz": gmid, "maxFreqKhz": gmax}

    # --- sysfs knobs -----------------------------------------------------
    knob = {k: v for k, v in c["modules"]["sysfs"]["knob"].items()
            if k.startswith("cpuset")}
    for i, (_, cpus, *_r) in enumerate(cl):
        base = f"/sys/devices/system/cpu/cpu{cpus[0]}/cpufreq"
        knob[f"c{i}MaxFreq"] = f"{base}/scaling_max_freq"
        knob[f"c{i}MinFreq"] = f"{base}/scaling_min_freq"
    knob["cpuSchedBoost"] = "/proc/sys/walt/sched_boost"
    c["modules"]["sysfs"]["knob"] = knob

    # --- cpumask / affinity ---------------------------------------------
    cm = {"all": list(range(8))}
    for i, (_, cpus, *_r) in enumerate(cl):
        cm[f"c{i}"] = cpus
    mids = [i for i, x in enumerate(cl) if x[0] == "mid"]
    prime = n - 1
    if len(mids) > 1:
        cm["mid"] = sorted(sum((cl[i][1] for i in mids), []))
        mid_name = "mid"
    else:
        mid_name = f"c{mids[0]}"
    prime_name = f"c{prime}"
    s = c["modules"]["sched"]
    s["cpumask"] = cm
    for cls in s["affinity"].values():
        for k, v in cls.items():
            if v == "c1": cls[k] = mid_name
            elif v == "c2": cls[k] = prime_name

    # Qualcomm thread names (best effort; MTK-only names are harmless)
    for r in s["rules"]:
        for tr in r["rules"]:
            if "ged-swd" in tr["k"]:
                tr["k"] = tr["k"].replace("ged-swd)", "ged-swd|kgsl_worker|adreno)")

    # --- initial cpusets --------------------------------------------------
    cs = p["cpuset"]
    ini = c["initials"]["sysfs"]
    ini.update(cpusetTa="0-7", cpusetFg=cs["Fg"], cpusetBg=cs["Bg"],
               cpusetSysBg=cs["SysBg"], cpusetRe=cs["Re"])

    # --- presets: rescale per-cluster frequencies ------------------------------
    maxes = [x[2] for x in cl]
    roles = [x[0] for x in cl]
    pat = re.compile(r"^c([0-2])MaxFreq$")
    for pname, preset in c["presets"].items():
        for hint, params in preset.items():
            new = {}
            for k, v in params.items():
                m = pat.match(k)
                if not m:
                    if k == "cpusetTa" and v == "0-6":
                        v = cs["idleTa"]
                    new[k] = v
                    continue
                role = TROLE[f"c{m.group(1)}"]
                ratio = int(v) / TMAX[role]
                for i, r in enumerate(roles):
                    if r == role:
                        khz = int(round(ratio * maxes[i] / 1000.0)) * 1000
                        if khz >= maxes[i] - 1000:
                            khz = maxes[i]
                        new[f"c{i}MaxFreq"] = str(khz)
            preset[hint] = new
    return c

for key, p in PLATFORMS.items():
    out = build(key, p)
    with open(f"config/{key}.json", "w") as f:
        json.dump(out, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print("wrote", key, len(out["modules"]["cpu"]["powerModel"]), "clusters")
