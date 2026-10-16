#!/bin/sh
# A light single-threaded step.
end=$(($(date +%s)+1)); while [ "$(date +%s)" -lt "$end" ]; do :; done
