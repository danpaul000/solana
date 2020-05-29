#!/usr/bin/env bash

set -e
#set -x

BUCKET=mainnet-beta-ledger-us-west1
ledger_root_path=/mnt/ledger-data-disk/$BUCKET/

existing_ledger_dirs="$(ls -d $ledger_root_path/*/)"

for ledger_dir in ${existing_ledger_dirs[@]}; do
  subdirs="$(cd $ledger_dir ; ls -d */)"
  already_extracted=false
  for subdir in $subdirs; do
    if [[ $subdir = rocksdb/ ]]; then
      already_extracted=true
      break
    fi
  done
  if [[ $already_extracted = false ]]; then
    echo Extracting $ledger_dir
    (cd $ledger_dir ; tar jxf rocksdb.tar.bz2 &)
  fi
done
