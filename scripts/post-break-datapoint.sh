#!/usr/bin/env bash

set -e

cd "$(dirname "$0")"
metricsWriteDatapoint="$(dirname "${BASH_SOURCE[0]}")"/metrics-write-datapoint.sh

ADDRESS="$1"
TRANSACTIONS="$2"

export INFLUX_HOST="https://metrics.solana.com:8086"
export INFLUX_DATABASE="break"
export INFLUX_USERNAME="break_writer"
export INFLUX_PASSWORD="secret_sauce"

point="total_transactions,address=$ADDRESS transactions=$TRANSACTIONS"

./metrics-write-datapoint.sh "$point"
