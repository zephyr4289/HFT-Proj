# 🏛️ Fleet Silicon Census (1,000 Shard Profiling Dataset)

This branch (`exp/fleet-silicon-census`) is a lightweight, dedicated data-collection engine designed to profile the hardware, CPU microarchitectures, and memory bandwidth distributions across the GitHub Actions runner fleet.

---

## 📂 Directory Structure

```text
.
├── .github/workflows/
│   └── census.yml             # 100-shard matrix workflow with manual dispatch
├── census_data/               # Persistent sequential run repository
│   ├── cumulative_census_dataset.jsonl   # Line-delimited JSON of all profiled hosts
│   ├── run_<run_num>_<run_id>/           # Dedicated per-run folder
│   │   ├── summary.json                  # Aggregated run statistics
│   │   └── shard_telemetry_<shard>.json  # Individual microarchitectural telemetry
│   └── .gitkeep
├── scripts/
│   ├── census_probe.sh        # Hardware inspector & telemetry extractor
│   └── probe.c                # High-speed streaming memory bandwidth probe
└── README.md
```

---

## ⚡ How to Trigger

1. **GitHub UI**: Navigate to **Actions** $\to$ **Fleet Silicon Census** $\to$ **Run workflow**.
2. **Git Push**: Pushing a commit without `[skip ci]` immediately launches a 100-shard batch.
3. **Target**: Running 10 consecutive batches populates **1,000 distinct hardware samples** into `census_data/cumulative_census_dataset.jsonl`.
