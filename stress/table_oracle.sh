#!/usr/bin/env bash
# Independent oracle for stress/table_digest.blsp: the identical LCG + op sequence over a
# bash associative array, so run.sh can compare two implementations that share no code.
# bash arithmetic is signed 64-bit; s < 2^31 keeps s * 1103515245 + 12345 below 2^61.
set -u
declare -A table=()
s=20260716
for ((step = 0; step < 200000; step++)); do
  s=$(( (s * 1103515245 + 12345) % 2147483648 ))
  choice=$(( s % 10 )); key=$(( s % 4096 )); value=$(( s % 100000 ))
  if (( choice < 6 )); then table[$key]=$value
  elif (( choice < 8 )); then unset "table[$key]"
  fi
done
count=0; sum_key_value=0; sum_key=0
for key in "${!table[@]}"; do
  count=$(( count + 1 ))
  sum_key_value=$(( sum_key_value + key * table[$key] ))
  sum_key=$(( sum_key + key ))
done
echo "digest $count $sum_key_value $sum_key $count"
