#!/usr/bin/env bash

# Quick and dirty script to query the mainnet beta cluster for
# all System and Stake account addresses and their balance

datestamp="$(date -u +%Y-%m-%d)"
timestamp="$(date -u +%H:%M:%S)"

outfile=accounts-addresses-and-balances-"$datestamp"_"$timestamp".csv
RPC_URL=http://api.mainnet-beta.solana.com:80


slot="$(curl -s -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1, "method":"getSlot"}' $RPC_URL | jq -r '.result')"

echo program,address,balance,date,time,approx_slot > $outfile

curl -s -X POST -H "Content-Type: application/json" -d \
  '{"jsonrpc":"2.0","id":1, "method":"getProgramAccounts", "params":["Stake11111111111111111111111111111111111111"]}' $RPC_URL \
  | jq -r --arg datestamp "$datestamp" --arg timestamp "$timestamp" --arg slot "$slot" '.result | .[] | ["STAKE", .pubkey, .account.lamports/1000000000, $datestamp, $timestamp, $slot] | @csv' \
  | sed 's/\"//g' >> $outfile

curl -s -X POST -H "Content-Type: application/json" -d \
  '{"jsonrpc":"2.0","id":1, "method":"getProgramAccounts", "params":["11111111111111111111111111111111"]}' $RPC_URL \
  | jq -r --arg datestamp "$datestamp" --arg timestamp "$timestamp" --arg slot "$slot" '.result | .[] | ["SYSTEM", .pubkey, .account.lamports/1000000000, $datestamp, $timestamp, $slot] | @csv' \
  | sed 's/\"//g' >> $outfile

curl -s -X POST -H "Content-Type: application/json" -d \
  '{"jsonrpc":"2.0","id":1, "method":"getProgramAccounts", "params":["Vote111111111111111111111111111111111111111"]}' $RPC_URL \
  | jq -r --arg datestamp "$datestamp" --arg timestamp "$timestamp" --arg slot "$slot" '.result | .[] | ["VOTE", .pubkey, .account.lamports/1000000000, $datestamp, $timestamp, $slot] | @csv' \
  | sed 's/\"//g' >> $outfile

echo Wrote data to $outfile