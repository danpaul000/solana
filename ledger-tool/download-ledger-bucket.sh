#!/usr/bin/env bash

set -e
#set -x

BUCKET=mainnet-beta-ledger-us-west1
ledger_root_path=/mnt/ledger-data-disk/$BUCKET/

existing_ledger_dirs="$(cd $ledger_root_path ; ls -d */)"

all_bucket_dirs="$(gsutil ls gs://$BUCKET)"

dirs_to_download=()
for bucket_dir in ${all_bucket_dirs[@]}; do
  [[ $(basename $bucket_dir | grep genesis) ]] && continue
  already_downloaded=false
  for ledger_dir in ${existing_ledger_dirs[@]}; do
    if [[ $(basename $bucket_dir) -eq $(basename $ledger_dir) ]]; then
      already_downloaded=true
      break
    fi
  done
  if [[ $already_downloaded = false ]]; then
    dirs_to_download+=($bucket_dir)
  fi
done

echo "All new dirs to download"
for d in ${dirs_to_download[@]}; do
  echo $d
  gsutil cp -r $d $ledger_root_path &
done
