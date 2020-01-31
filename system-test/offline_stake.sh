#!/usr/bin/env bash

set -x

url="$1"
[[ -n $url ]] || url=http://0.0.0.0:8899

solana config set --url "$url"

### Offline stake delegation

# Create a system account and airdrop it 100 SOL
offline_system_account_keypair_file=offline_system_account_keypair.json
solana-keygen new -o "$offline_system_account_keypair_file" --no-passphrase --force
offline_system_account_pubkey="$(solana-keygen pubkey $offline_system_account_keypair_file)"
solana airdrop 100 SOL -k $offline_system_account_keypair_file

online_system_account_keypair_file=online_system_account_keypair.json
solana-keygen new -o "$online_system_account_keypair_file" --no-passphrase --force
online_system_account_pubkey="$(solana-keygen pubkey $online_system_account_keypair_file)"
solana airdrop 100 SOL -k $online_system_account_keypair_file


# Create a stake account with 50 SOL at a seeded address associated with the system account pubkey
seed="foo"
stake_account_address="$(solana create-address-with-seed $seed STAKE --from $offline_system_account_keypair_file -k $offline_system_account_keypair_file)"
solana create-stake-account $offline_system_account_keypair_file --seed $seed 50 SOL -k $offline_system_account_keypair_file

## Create a vote account for staking
#vote_account_keypair_file=vote_account_keypair.json
#solana-keygen new -o "$vote_account_keypair_file" --no-passphrase --force
#solana create-vote-account $vote_account_keypair_file $offline_system_account_keypair_file -k $offline_system_account_keypair_file
#vote_account_pubkey="$(solana-keygen pubkey $vote_account_keypair_file)"

# Use bootstrap's vote account for staking
vote_account_pubkey=rpFoaK9tDzjh2SwBV7zYxVEQjj4y3TVDcQpEvbShC1V

# Create a nonce account with the system account as the nonce authority
nonce_account_keypair_file=nonce_keypair.json
solana-keygen new -o "$nonce_account_keypair_file" --no-passphrase --force
nonce_account_pubkey="$(solana-keygen pubkey $nonce_account_keypair_file)"
solana create-nonce-account $nonce_account_keypair_file 1 SOL --nonce-authority $offline_system_account_pubkey -k $offline_system_account_keypair_file
nonce="$(solana nonce $nonce_account_keypair_file)"

# Sign a stake delegation offline, assuming the system account keypair file (authorized staker) is held offline
sign_only="$(solana delegate-stake --blockhash $nonce --nonce $nonce_account_pubkey --nonce-authority $offline_system_account_pubkey --stake-authority $offline_system_account_pubkey $stake_account_address $vote_account_pubkey -k $offline_system_account_keypair_file --sign-only)"
signers=()
while read LINE; do
  signers+=( --signer $LINE )
done <<<"$(sed -Ee $'s/^  ([a-zA-Z0-9]+=[a-zA-Z0-9]+)/\\1/\nt\nd' <<<"$sign_only")"

# Send the signed transaction on the cluster
solana delegate-stake --blockhash $nonce --nonce $nonce_account_pubkey --nonce-authority $offline_system_account_pubkey --stake-authority $offline_system_account_pubkey $stake_account_address $vote_account_pubkey --fee-payer $offline_system_account_pubkey ${signers[@]}
