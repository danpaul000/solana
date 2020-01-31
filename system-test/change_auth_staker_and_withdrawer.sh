#!/usr/bin/env bash

# Set up authorized staker and withdrawer accounts.
# staker_one and withdrawer_one are offline accounts.
# Transfer authority to staker_two and withdrawer_two, which are offline accounts

staker_one_keypair=staker_one.json
staker_two_keypair=staker_two.json
withdrawer_one_keypair=withdrawer_one.json
withdrawer_two_keypair=withdrawer_two.json

solana-keygen new -o $staker_one_keypair --force --no-passphrase --silent
solana-keygen new -o $staker_two_keypair --force --no-passphrase --silent
solana-keygen new -o $withdrawer_one_keypair --force --no-passphrase --silent
solana-keygen new -o $withdrawer_two_keypair --force --no-passphrase --silent

staker_one_pubkey="$(solana-keygen pubkey $staker_one_keypair)"
staker_two_pubkey="$(solana-keygen pubkey $staker_two_keypair)"
withdrawer_one_pubkey="$(solana-keygen pubkey $withdrawer_one_keypair)"
withdrawer_two_pubkey="$(solana-keygen pubkey $withdrawer_two_keypair)"

solana airdrop 1 SOL -k $staker_one_keypair
solana airdrop 1 SOL -k $staker_two_keypair
solana airdrop 1 SOL -k $withdrawer_one_keypair
solana airdrop 1 SOL -k $withdrawer_two_keypair

# Set up a funding account and a stake address

community_keypair=community.json
stake_account_keypair=stake_account.json

solana-keygen new -o "$community_keypair" --no-passphrase --force --silent
solana-keygen new -o "$stake_account_keypair" --no-passphrase --force --silent

community_pubkey="$(solana-keygen pubkey $community_keypair)"
stake_account="$(solana-keygen pubkey $stake_account_keypair)"

solana airdrop 100 SOL -k $community_keypair

# Create stake account

echo Staker pubkey $staker_one_pubkey
echo Withdrawer pubkey $withdrawer_one_pubkey

# TODO: In production, instead of -k/--keypair we should pass --ask-seed-phrase to keep the funding community keypair offline
solana create-stake-account $stake_account_keypair 10 SOL -k $community_keypair --stake-authority $staker_one_pubkey --withdraw-authority $withdrawer_one_pubkey
solana stake-account $stake_account

# Create a nonce account with an online nonce authority account
nonce_authority_keypair=nonce_authority_keypair.json
solana-keygen new -o $nonce_authority_keypair --no-passphrase --force --silent
nonce_authority_pubkey="$(solana-keygen pubkey $nonce_authority_keypair)"
solana airdrop 2 SOL -k $nonce_authority_keypair

nonce_account_keypair=nonce_keypair.json
solana-keygen new -o $nonce_account_keypair --no-passphrase --force --silent
nonce_account_pubkey="$(solana-keygen pubkey $nonce_account_keypair)"
solana create-nonce-account $nonce_account_keypair 1 SOL -k $nonce_authority_keypair --nonce-authority $nonce_account_pubkey
nonce="$(solana nonce $nonce_account_keypair)"

# Set a new auth staker to staker two

# OFFLINE
sign_only="$(solana stake-authorize-staker $stake_account $staker_two_pubkey -k $staker_one_keypair --stake-authority $staker_one_keypair \
--sign-only --blockhash $nonce --nonce $nonce_account_pubkey --nonce-authority $nonce_authority_keypair)"

signers=()
while read LINE; do
  signers+=( --signer $LINE )
done <<<"$(sed -Ee $'s/^  ([a-zA-Z0-9]+=[a-zA-Z0-9]+)/\\1/\nt\nd' <<<"$sign_only")"

# ONLINE
solana stake-authorize-staker $stake_account $staker_two_pubkey -k $staker_one_keypair --stake-authority $staker_one_keypair \
--blockhash $nonce --nonce $nonce_account_pubkey --nonce-authority $nonce_authority_keypair ${signers[@]}


solana stake-account $stake_account

# Set a new auth withdrawer to withdrawer two
solana stake-authorize-withdrawer $stake_account $withdrawer_two_pubkey -k $community_keypair --withdraw-authority $withdrawer_one_keypair
solana stake-account $stake_account

