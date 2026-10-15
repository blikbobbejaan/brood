# shellcheck shell=sh
# The address-space cap every test-running step sits behind (CLAUDE.md "Build uncapped, run
# capped"): one value, sourced by the gates and the pre-push hook and read by the Makefile.
# A diverging process is indistinguishable from a heavy one until it has eaten the machine
# (KI-87); under the cap it dies in seconds with its panic site named. 16 GB, not less: the
# runtime reserves ~3 GB of address space before doing any work. Never put it in front of a
# build — the linker inherits it and dies with `LLVM ERROR: out of memory`.
RUN_CAP_KB=16000000
