#!/usr/bin/env bash
# ASLR and PID normalization filter for kernel strace log diffing (doc 08 §10, PR-3 Stage 2)

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <input_strace_file> <output_normalized_file>"
    exit 1
fi

INPUT="$1"
OUTPUT="$2"

sed -E 's/0x[0-9a-f]+/0xADDR/g; s/^[0-9]+ //' "$INPUT" > "$OUTPUT"
