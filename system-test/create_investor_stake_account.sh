#!/usr/bin/env bash

solana-keygen new -o auth_staker.json --force --no-passphrase --silent
auth_staker_pubkey="$(solana-keygen pubkey auth_staker.json)"

solana-keygen new -o auth_withdrawer.json --force --no-passphrase --silent
auth_withdrawer_pubkey="$(solana-keygen pubkey auth_withdrawer.json)"

solana-keygen new -o community_keypair.json --force --no-passphrase --silent
solana airdrop 100 SOL -k cluttered_complaint.json

solana-keygen new -o investor_keypair.json --force --no-passphrase
investor_pubkey="$(solana-keygen pubkey investor_keypair.json)"
investor_stake_address="$(solana create-address-with-seed --from $investor_pubkey 0 STAKE)"

solana create-stake-account -k cluttered_complaint.json --stake-authority $auth_staker_pubkey --withdraw-authority $auth_withdrawer_pubkey --seed 0 investor_keypair.json 10 SOL

solana stake-account $investor_stake_address
