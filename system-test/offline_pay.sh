#!/usr/bin/env bash

set -x

url="$1"
[[ -n $url ]] || url=http://0.0.0.0:8899

solana config set --url "$url"

### Offline payment.
# Assuming that community keypair is owned by someone on the Solana team and is stored offline
# Problem: No way to distinguish fee payer (online) from sender (offline).  Currently this requires passing -k [desired_offline_keypair_file] to the online part, which defeats the security of offline signing

community_keypair_file=community_keypair.json
user_account_keypair_file=user_account_keypair.json
nonce_account_keypair_file=nonce_keypair.json

solana-keygen new -o "$community_keypair_file" --no-passphrase --force
solana-keygen new -o "$user_account_keypair_file" --no-passphrase --force
solana-keygen new -o "$nonce_account_keypair_file" --no-passphrase --force

community_pubkey="$(solana-keygen pubkey $community_keypair_file)"
user_account_pubkey="$(solana-keygen pubkey $user_account_keypair_file)"
nonce_account_pubkey="$(solana-keygen pubkey $nonce_account_keypair_file)"

solana airdrop 100 SOL -k "$community_keypair_file"
solana create-nonce-account "$nonce_account_keypair_file" 1 SOL -k "$community_keypair_file"

nonce="$(solana nonce $nonce_account_keypair_file)"

# Sign the payment transaction offline
sign_only="$(solana pay $user_account_pubkey 10 SOL --sign-only --blockhash $nonce --nonce $nonce_account_pubkey -k $community_keypair_file)"
signers=()
while read LINE; do
  signers+=( --signer $LINE )
done <<<"$(sed -Ee $'s/^  ([a-zA-Z0-9]+=[a-zA-Z0-9]+)/\\1/\nt\nd' <<<"$sign_only")"

# Submit the payment to the cluster
solana pay $user_account_pubkey 10 SOL --blockhash $nonce --nonce $nonce_account_pubkey --keypair $community_keypair_file ${signers[@]}


# This should work, once Trent's fixes are in
#solana pay $user_account_pubkey 10 SOL --blockhash $nonce --nonce $nonce_account_pubkey --fee-payer $community_pubkey ${signers[@]}

