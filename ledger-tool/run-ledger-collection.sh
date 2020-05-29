#!/usr/bin/env bash

set -e
#set -x

cd "$(dirname "$0")"

ledger_tool=/home/dan/sr/target/release/solana-ledger-tool

ledger_root_path=/mnt/ledger-data-disk/mainnet-beta-ledger-us-west1

timestamp="$(date -u +"%Y-%m-%d_%H:%M:%S")"
collection_dir="ledger_collection_${timestamp}"
mkdir -p $collection_dir

all_txns_file="${collection_dir}/all_transactions.csv"
all_instructions_file="${collection_dir}/all_instructions.csv"

txn_headers="slot,cluster_unix_timestamp,recent_blockhash,txn_sig,fee,num_instructions,num_accounts,\
account_0_pubkey,account_0_pre_balance,account_0_post_balance,\
account_1_pubkey,account_1_pre_balance,account_1_post_balance,\
account_2_pubkey,account_2_pre_balance,account_2_post_balance,\
account_3_pubkey,account_3_pre_balance,account_3_post_balance,\
account_4_pubkey,account_4_pre_balance,account_4_post_balance,\
account_5_pubkey,account_5_pre_balance,account_5_post_balance,\
account_6_pubkey,account_6_pre_balance,account_6_post_balance,\
account_7_pubkey,account_7_pre_balance,account_7_post_balance,\
account_8_pubkey,account_8_pre_balance,account_8_post_balance,\
account_9_pubkey,account_9_pre_balance,account_9_post_balance"

echo "$txn_headers" > $all_txns_file

instruction_headers="txn_sig,program_pubkey,program_instruction,\
instruction_account_0,\
instruction_account_1,\
instruction_account_2,\
instruction_account_3,\
instruction_account_4,\
instruction_account_5,\
instruction_account_6,\
instruction_account_7,\
instruction_account_8,\
instruction_account_9"

echo "$instruction_headers" > $all_instructions_file

previous_end_slot="$1"
[[ -n $previous_end_slot ]] || previous_end_slot=0

dirs="$(ls -d $ledger_root_path/*/ | sort -n -t / -k 5)"

for ledger_dir in ${dirs[@]}; do
  echo "-----------------------"

  bounds_str=$($ledger_tool -l "$ledger_dir" bounds)

  if [[ -n "$(echo "$bounds_str" | grep "Ledger has data for slots")" ]]; then
    start_slot="$(echo "$bounds_str" | awk '{ print $6 }')"
    end_slot="$(echo "$bounds_str" | awk '{ print $8 }')"
    echo "Ledger in $ledger_dir has data from slot $start_slot to $end_slot"

    if [[ $previous_end_slot -gt $end_slot ]] ; then
      echo "Previous iteration ended at a higher slot than this ledger contains.  Skipping it."
      continue
#    elif [[ $previous_end_slot -gt $start_slot ]] ; then
#      start_slot=$((previous_end_slot + 1))
#      echo "Previous iteration ended at $previous_end_slot.  Starting current iteration at $start_slot to avoid overlap."
    fi

    transactions_file="${collection_dir}/transactions_${start_slot}_to_${end_slot}.csv"
    instructions_file="${collection_dir}/instructions_${start_slot}_to_${end_slot}.csv"

    write_ledger_to_csv_cmd="$ledger_tool -l $ledger_dir print-csv --transaction-csv-file $transactions_file --instruction-csv-file $instructions_file --starting-slot $start_slot"

    ( set -x ; $write_ledger_to_csv_cmd )

    echo "Appending $transactions_file to $all_txns_file"
    cat $transactions_file >> $all_txns_file

    echo "Appending $instructions_file to $all_instructions_file"
    cat $instructions_file >> $all_instructions_file

    previous_end_slot=$end_slot

  else
    echo "Ledger in $ledger_dir has no data!"
    continue
  fi
done
