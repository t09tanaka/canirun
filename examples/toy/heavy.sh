#!/bin/sh
# Pretend to be a parallel test suite: 3 workers busy for 4 seconds.
for _ in 1 2 3; do
  ( end=$(($(date +%s)+4)); while [ "$(date +%s)" -lt "$end" ]; do :; done ) &
done
wait
