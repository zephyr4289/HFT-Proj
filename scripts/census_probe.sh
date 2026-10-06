#!/usr/bin/env bash
set -euo pipefail

SHARD="${1:-1}"
OUTPUT_DIR="${2:-/tmp/shard_output}"
mkdir -p "$OUTPUT_DIR"

echo "=== PROBING HARDWARE TELEMETRY (Shard ${SHARD}) ==="

# 1. CPU Core Info
CPU_NAME=$(grep -m1 "model name" /proc/cpuinfo | cut -d: -f2 | xargs || echo "Unknown CPU")
VENDOR_ID=$(grep -m1 "vendor_id" /proc/cpuinfo | cut -d: -f2 | xargs || echo "Unknown Vendor")
CPU_FAMILY=$(grep -m1 "cpu family" /proc/cpuinfo | cut -d: -f2 | xargs || echo "0")
CPU_MODEL=$(grep -m1 "model" /proc/cpuinfo | grep -v "model name" | cut -d: -f2 | xargs || echo "0")
CPU_STEPPING=$(grep -m1 "stepping" /proc/cpuinfo | cut -d: -f2 | xargs || echo "0")
CPU_MHZ=$(grep -m1 "cpu MHz" /proc/cpuinfo | cut -d: -f2 | xargs || echo "0.0")
CPU_CORES=$(grep -c "^processor" /proc/cpuinfo || echo "1")

# 2. DMI / Azure VM SKU Info
DMI_SYS_VENDOR=$(cat /sys/devices/virtual/dmi/id/sys_vendor 2>/dev/null || echo "Unknown")
DMI_PRODUCT_NAME=$(cat /sys/devices/virtual/dmi/id/product_name 2>/dev/null || echo "Unknown SKU")

# 3. Cache Hierarchy
CACHE_L1D=$(lscpu | grep "L1d cache:" | awk '{$1=$2=""; print $0}' | xargs || echo "N/A")
CACHE_L1I=$(lscpu | grep "L1i cache:" | awk '{$1=$2=""; print $0}' | xargs || echo "N/A")
CACHE_L2=$(lscpu | grep "L2 cache:" | awk '{$1=$2=""; print $0}' | xargs || echo "N/A")
CACHE_L3=$(lscpu | grep "L3 cache:" | awk '{$1=$2=""; print $0}' | xargs || echo "N/A")
NUMA_NODES=$(lscpu | grep "NUMA node(s):" | awk '{print $NF}' || echo "1")

# 4. ISA & Vector Feature Flags
FLAGS=$(grep -m1 "flags" /proc/cpuinfo | cut -d: -f2 || echo "")
has_flag() { echo "$FLAGS" | grep -qw "$1" && echo "true" || echo "false"; }

HAS_AVX512F=$(has_flag "avx512f")
HAS_AVX512BW=$(has_flag "avx512bw")
HAS_AVX512DQ=$(has_flag "avx512dq")
HAS_VPCLMULQDQ=$(has_flag "vpclmulqdq")
HAS_VAES=$(has_flag "vaes")
HAS_GFNI=$(has_flag "gfni")
HAS_AVX2=$(has_flag "avx2")
HAS_AMX_TILE=$(has_flag "amx_tile")
HAS_AMX_INT8=$(has_flag "amx_int8")
HAS_CONSTANT_TSC=$(has_flag "constant_tsc")
HAS_NONSTOP_TSC=$(has_flag "nonstop_tsc")

# 5. Compile and run fast C memory bandwidth probe
gcc -O3 -march=native "$(dirname "$0")/probe.c" -o /tmp/probe -lpthread 2>/dev/null || true
BANDWIDTH_1T="0.0"
if [ -f /tmp/probe ]; then
  BW_OUT=$(/tmp/probe || echo "BANDWIDTH_1T_GB_S=0.0")
  BANDWIDTH_1T=$(echo "$BW_OUT" | grep "BANDWIDTH_1T_GB_S=" | cut -d= -f2 || echo "0.0")
fi

echo "Host CPU: $CPU_NAME ($VENDOR_ID)"
echo "Azure SKU: $DMI_PRODUCT_NAME"
echo "Bandwidth 1T: $BANDWIDTH_1T GB/s"
echo "AVX-512: $HAS_AVX512F | VPCLMULQDQ: $HAS_VPCLMULQDQ | GFNI: $HAS_GFNI"

# 6. Emit Structured JSON
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
cat <<JSON > "$OUTPUT_DIR/shard_telemetry_${SHARD}.json"
{
  "shard": ${SHARD},
  "timestamp": "${TIMESTAMP}",
  "cpu": {
    "model_name": "${CPU_NAME}",
    "vendor_id": "${VENDOR_ID}",
    "family": ${CPU_FAMILY},
    "model": ${CPU_MODEL},
    "stepping": ${CPU_STEPPING},
    "mhz": ${CPU_MHZ},
    "vcpus": ${CPU_CORES},
    "numa_nodes": ${NUMA_NODES}
  },
  "dmi": {
    "sys_vendor": "${DMI_SYS_VENDOR}",
    "product_name": "${DMI_PRODUCT_NAME}"
  },
  "cache": {
    "l1d": "${CACHE_L1D}",
    "l1i": "${CACHE_L1I}",
    "l2": "${CACHE_L2}",
    "l3": "${CACHE_L3}"
  },
  "isa": {
    "avx2": ${HAS_AVX2},
    "avx512f": ${HAS_AVX512F},
    "avx512bw": ${HAS_AVX512BW},
    "avx512dq": ${HAS_AVX512DQ},
    "vpclmulqdq": ${HAS_VPCLMULQDQ},
    "vaes": ${HAS_VAES},
    "gfni": ${HAS_GFNI},
    "amx_tile": ${HAS_AMX_TILE},
    "amx_int8": ${HAS_AMX_INT8},
    "constant_tsc": ${HAS_CONSTANT_TSC},
    "nonstop_tsc": ${HAS_NONSTOP_TSC}
  },
  "memory_benchmark": {
    "bandwidth_1t_gb_s": ${BANDWIDTH_1T}
  }
}
JSON

echo "Wrote telemetry to $OUTPUT_DIR/shard_telemetry_${SHARD}.json"
