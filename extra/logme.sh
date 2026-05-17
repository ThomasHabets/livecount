#!/usr/bin/env bash
set -ueo pipefail

HOST="$1"

#while true; do
        ACTIVE="$(curl -s --max-time 10 https://${HOST}/livecount/metrics | grep ^total_active | awk '{print $2}')"
        TS="$(date +%s)"
        echo "$TS $ACTIVE"
#        sleep 10
#done
