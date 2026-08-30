#!/usr/bin/env bash
set -e

# Setup veth pair for AF_XDP transport CI testing (doc 09 §9)
ip link add v0 type veth peer name v1 2>/dev/null || true
ip link set v0 up
ip link set v1 up

if [ -f xdp/redirect.o ]; then
  ip link set dev v1 xdp obj xdp/redirect.o sec xdp 2>/dev/null || true
fi
