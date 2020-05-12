use crate::{
    checks::{check_account_for_fee, check_unique_pubkeys},
    cli::{
        fee_payer_arg, generate_unique_signers, log_instruction_custom_error, nonce_authority_arg,
        return_signers, CliCommand, CliCommandInfo, CliConfig, CliError, ProcessResult,
        SignerIndex, FEE_PAYER_ARG,
    },
    cli_output::{CliStakeHistory, CliStakeHistoryEntry, CliStakeState, CliStakeType},
    nonce::{check_nonce_account, nonce_arg, NONCE_ARG, NONCE_AUTHORITY_ARG},
    offline::{blockhash_query::BlockhashQuery, *},
    spend_utils::{resolve_spend_tx_and_check_account_balances, SpendAmount},
};
use clap::{App, Arg, ArgGroup, ArgMatches, SubCommand};
use solana_clap_utils::{input_parsers::*, input_validators::*, offline::*, ArgConstant};
use solana_client::{rpc_client::RpcClient, rpc_request::DELINQUENT_VALIDATOR_SLOT_DISTANCE};
use solana_remote_wallet::remote_wallet::RemoteWalletManager;
use solana_sdk::{
    account_utils::StateMut,
    clock::Clock,
    message::Message,
    pubkey::Pubkey,
    system_instruction::SystemError,
    sysvar::{
        clock,
        stake_history::{self, StakeHistory},
        Sysvar,
    },
    transaction::Transaction,
};
use solana_stake_program::{
    stake_instruction::{self, LockupArgs, StakeError},
    stake_state::{Authorized, Lockup, Meta, StakeAuthorize, StakeState},
};
use solana_vote_program::vote_state::VoteState;
use std::{collections::HashSet, ops::Deref, sync::Arc};

pub const STAKE_AUTHORITY_ARG: ArgConstant<'static> = ArgConstant {
    name: "stake_authority",
    long: "stake-authority",
    help: "Authorized staker [default: cli config keypair]",
};

pub const WITHDRAW_AUTHORITY_ARG: ArgConstant<'static> = ArgConstant {
    name: "withdraw_authority",
    long: "withdraw-authority",
    help: "Authorized withdrawer [default: cli config keypair]",
};

fn stake_authority_arg<'a, 'b>() -> Arg<'a, 'b> {
    Arg::with_name(STAKE_AUTHORITY_ARG.name)
        .long(STAKE_AUTHORITY_ARG.long)
        .takes_value(true)
        .value_name("KEYPAIR")
        .validator(is_valid_signer)
        .help(STAKE_AUTHORITY_ARG.help)
}

fn withdraw_authority_arg<'a, 'b>() -> Arg<'a, 'b> {
    Arg::with_name(WITHDRAW_AUTHORITY_ARG.name)
        .long(WITHDRAW_AUTHORITY_ARG.long)
        .takes_value(true)
        .value_name("KEYPAIR")
        .validator(is_valid_signer)
        .help(WITHDRAW_AUTHORITY_ARG.help)
}

pub trait StakeSubCommands {
    fn stake_subcommands(self) -> Self;
}

impl StakeSubCommands for App<'_, '_> {
    fn stake_subcommands(self) -> Self {
        self.subcommand(
            SubCommand::with_name("create-stake-account")
                .about("Create a stake account")
                .arg(
                    Arg::with_name("stake_account")
                        .index(1)
                        .value_name("ACCOUNT_KEYPAIR")
                        .takes_value(true)
                        .required(true)
                        .validator(is_valid_signer)
                        .help("Stake account to create (or base of derived address if --seed is used)")
                )
                .arg(
                    Arg::with_name("amount")
                        .index(2)
                        .value_name("AMOUNT")
                        .takes_value(true)
                        .validator(is_amount_or_all)
                        .required(true)
                        .help("The amount to send to the stake account, in SOL; accepts keyword ALL")
                )
                .arg(
                    pubkey!(Arg::with_name("custodian")
                        .long("custodian")
                        .value_name("PUBKEY"),
                        "Authority to modify lockups. ")
                )
                .arg(
                    Arg::with_name("seed")
                        .long("seed")
                        .value_name("STRING")
                        .takes_value(true)
                        .help("Seed for address generation; if specified, the resulting account will be at a derived address of the stake_account pubkey")
                )
                .arg(
                    Arg::with_name("lockup_epoch")
                        .long("lockup-epoch")
                        .value_name("NUMBER")
                        .takes_value(true)
                        .help("The epoch height at which this account will be available for withdrawal")
                )
                .arg(
                    Arg::with_name("lockup_date")
                        .long("lockup-date")
                        .value_name("RFC3339 DATETIME")
                        .validator(is_rfc3339_datetime)
                        .takes_value(true)
                        .help("The date and time at which this account will be available for withdrawal")
                )
                .arg(
                    Arg::with_name(STAKE_AUTHORITY_ARG.name)
                        .long(STAKE_AUTHORITY_ARG.long)
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .validator(is_valid_pubkey)
                        .help(STAKE_AUTHORITY_ARG.help)
                )
                .arg(
                    Arg::with_name(WITHDRAW_AUTHORITY_ARG.name)
                        .long(WITHDRAW_AUTHORITY_ARG.long)
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .validator(is_valid_pubkey)
                        .help(WITHDRAW_AUTHORITY_ARG.help)
                )
                .arg(
                    Arg::with_name("from")
                        .long("from")
                        .takes_value(true)
                        .value_name("KEYPAIR")
                        .validator(is_valid_signer)
                        .help("Source account of funds [default: cli config keypair]"),
                )
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("delegate-stake")
                .about("Delegate stake to a vote account")
                .arg(
                    Arg::with_name("force")
                        .long("force")
                        .takes_value(false)
                        .hidden(true) // Don't document this argument to discourage its use
                        .help("Override vote account sanity checks (use carefully!)")
                )
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Stake account to delegate")
                )
                .arg(
                    pubkey!(Arg::with_name("vote_account_pubkey")
                        .index(2)
                        .value_name("VOTE_ACCOUNT_ADDRESS")
                        .required(true),
                        "The vote account to which the stake will be delegated")
                )
                .arg(stake_authority_arg())
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("stake-authorize")
                .about("Authorize a new signing keypair for the given stake account")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .required(true)
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS"),
                        "Stake account in which to set a new authority. ")
                )
                .arg(
                    pubkey!(Arg::with_name("new_stake_authority")
                        .long("new-stake-authority")
                        .required_unless("new_withdraw_authority")
                        .value_name("PUBKEY"),
                        "New authorized staker")
                )
                .arg(
                    pubkey!(Arg::with_name("new_withdraw_authority")
                        .long("new-withdraw-authority")
                        .required_unless("new_stake_authority")
                        .value_name("PUBKEY"),
                        "New authorized withdrawer. ")
                )
                .arg(stake_authority_arg())
                .arg(withdraw_authority_arg())
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("deactivate-stake")
                .about("Deactivate the delegated stake from the stake account")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Stake account to be deactivated. ")
                )
                .arg(stake_authority_arg())
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("split-stake")
                .about("Duplicate a stake account, splitting the tokens between the two")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Stake account to split (or base of derived address if --seed is used). ")
                )
                .arg(
                    Arg::with_name("split_stake_account")
                        .index(2)
                        .value_name("ACCOUNT_KEYPAIR")
                        .takes_value(true)
                        .required(true)
                        .validator(is_valid_signer)
                        .help("Keypair of the new stake account")
                )
                .arg(
                    Arg::with_name("amount")
                        .index(3)
                        .value_name("AMOUNT")
                        .takes_value(true)
                        .validator(is_amount)
                        .required(true)
                        .help("The amount to move into the new stake account, in SOL")
                )
                .arg(
                    Arg::with_name("seed")
                        .long("seed")
                        .value_name("STRING")
                        .takes_value(true)
                        .help("Seed for address generation; if specified, the resulting account will be at a derived address of the SPLIT STAKE ACCOUNT pubkey")
                )
                .arg(stake_authority_arg())
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("merge-stake")
                .about("Merges one stake account into another")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Stake account to merge into")
                )
                .arg(
                    pubkey!(Arg::with_name("source_stake_account_pubkey")
                        .index(2)
                        .value_name("SOURCE_STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Source stake account for the merge.  If successful, this stake account will no longer exist after the merge")
                )
                .arg(stake_authority_arg())
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("withdraw-stake")
                .about("Withdraw the unstaked SOL from the stake account")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Stake account from which to withdraw")
                )
                .arg(
                    pubkey!(Arg::with_name("destination_account_pubkey")
                        .index(2)
                        .value_name("RECIPIENT_ADDRESS")
                        .required(true),
                        "Recipient of withdrawn SOL")
                )
                .arg(
                    Arg::with_name("amount")
                        .index(3)
                        .value_name("AMOUNT")
                        .takes_value(true)
                        .validator(is_amount)
                        .required(true)
                        .help("The amount to withdraw from the stake account, in SOL")
                )
                .arg(withdraw_authority_arg())
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
                .arg(
                    Arg::with_name("custodian")
                        .long("custodian")
                        .takes_value(true)
                        .value_name("KEYPAIR")
                        .validator(is_valid_signer)
                        .help("Authority to override account lockup")
                )
        )
        .subcommand(
            SubCommand::with_name("stake-set-lockup")
                .about("Set Lockup for the stake account")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "Stake account for which to set lockup parameters. ")
                )
                .arg(
                    Arg::with_name("lockup_epoch")
                        .long("lockup-epoch")
                        .value_name("NUMBER")
                        .takes_value(true)
                        .help("The epoch height at which this account will be available for withdrawal")
                )
                .arg(
                    Arg::with_name("lockup_date")
                        .long("lockup-date")
                        .value_name("RFC3339 DATETIME")
                        .validator(is_rfc3339_datetime)
                        .takes_value(true)
                        .help("The date and time at which this account will be available for withdrawal")
                )
                .arg(
                    pubkey!(Arg::with_name("new_custodian")
                        .long("new-custodian")
                        .value_name("PUBKEY"),
                        "Identity of a new lockup custodian. ")
                )
                .group(ArgGroup::with_name("lockup_details")
                    .args(&["lockup_epoch", "lockup_date", "new_custodian"])
                    .multiple(true)
                    .required(true))
                .arg(
                    Arg::with_name("custodian")
                        .long("custodian")
                        .takes_value(true)
                        .value_name("KEYPAIR")
                        .validator(is_valid_signer)
                        .help("Keypair of the existing custodian [default: cli config pubkey]")
                )
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg())
        )
        .subcommand(
            SubCommand::with_name("stake-account")
                .about("Show the contents of a stake account")
                .alias("show-stake-account")
                .arg(
                    pubkey!(Arg::with_name("stake_account_pubkey")
                        .index(1)
                        .value_name("STAKE_ACCOUNT_ADDRESS")
                        .required(true),
                        "The stake account to display. ")
                )
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .takes_value(false)
                        .help("Display balance in lamports instead of SOL")
                )
        )
        .subcommand(
            SubCommand::with_name("stake-history")
                .about("Show the stake history")
                .alias("show-stake-history")
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .takes_value(false)
                        .help("Display balance in lamports instead of SOL")
                )
        )
    }
}

pub fn parse_stake_create_account(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let seed = matches.value_of("seed").map(|s| s.to_string());
    let epoch = value_of(matches, "lockup_epoch").unwrap_or(0);
    let unix_timestamp = unix_timestamp_from_rfc3339_datetime(matches, "lockup_date").unwrap_or(0);
    let custodian = pubkey_of_signer(matches, "custodian", wallet_manager)?.unwrap_or_default();
    let staker = pubkey_of_signer(matches, STAKE_AUTHORITY_ARG.name, wallet_manager)?;
    let withdrawer = pubkey_of_signer(matches, WITHDRAW_AUTHORITY_ARG.name, wallet_manager)?;
    let amount = SpendAmount::new_from_matches(matches, "amount");
    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of_signer(matches, NONCE_ARG.name, wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;
    let (from, from_pubkey) = signer_of(matches, "from", wallet_manager)?;
    let (stake_account, stake_account_pubkey) =
        signer_of(matches, "stake_account", wallet_manager)?;

    let mut bulk_signers = vec![fee_payer, from, stake_account];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::CreateStakeAccount {
            stake_account: signer_info.index_of(stake_account_pubkey).unwrap(),
            seed,
            staker,
            withdrawer,
            lockup: Lockup {
                custodian,
                epoch,
                unix_timestamp,
            },
            amount,
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
            from: signer_info.index_of(from_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_stake_delegate_stake(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();
    let vote_account_pubkey =
        pubkey_of_signer(matches, "vote_account_pubkey", wallet_manager)?.unwrap();
    let force = matches.is_present("force");
    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);
    let (stake_authority, stake_authority_pubkey) =
        signer_of(matches, STAKE_AUTHORITY_ARG.name, wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;

    let mut bulk_signers = vec![stake_authority, fee_payer];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::DelegateStake {
            stake_account_pubkey,
            vote_account_pubkey,
            stake_authority: signer_info.index_of(stake_authority_pubkey).unwrap(),
            force,
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_stake_authorize(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();

    let mut new_authorizations = Vec::new();
    let mut bulk_signers = Vec::new();
    if let Some(new_authority_pubkey) =
        pubkey_of_signer(matches, "new_stake_authority", wallet_manager)?
    {
        let (authority, authority_pubkey) = {
            let (authority, authority_pubkey) =
                signer_of(matches, STAKE_AUTHORITY_ARG.name, wallet_manager)?;
            // Withdraw authority may also change the staker
            if authority.is_none() {
                signer_of(matches, WITHDRAW_AUTHORITY_ARG.name, wallet_manager)?
            } else {
                (authority, authority_pubkey)
            }
        };
        new_authorizations.push((
            StakeAuthorize::Staker,
            new_authority_pubkey,
            authority_pubkey,
        ));
        bulk_signers.push(authority);
    };
    if let Some(new_authority_pubkey) =
        pubkey_of_signer(matches, "new_withdraw_authority", wallet_manager)?
    {
        let (authority, authority_pubkey) =
            signer_of(matches, WITHDRAW_AUTHORITY_ARG.name, wallet_manager)?;
        new_authorizations.push((
            StakeAuthorize::Withdrawer,
            new_authority_pubkey,
            authority_pubkey,
        ));
        bulk_signers.push(authority);
    };
    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;

    bulk_signers.push(fee_payer);
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    let new_authorizations = new_authorizations
        .into_iter()
        .map(
            |(stake_authorize, new_authority_pubkey, authority_pubkey)| {
                (
                    stake_authorize,
                    new_authority_pubkey,
                    signer_info.index_of(authority_pubkey).unwrap(),
                )
            },
        )
        .collect();

    Ok(CliCommandInfo {
        command: CliCommand::StakeAuthorize {
            stake_account_pubkey,
            new_authorizations,
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_split_stake(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();
    let (split_stake_account, split_stake_account_pubkey) =
        signer_of(matches, "split_stake_account", wallet_manager)?;
    let lamports = lamports_of_sol(matches, "amount").unwrap();
    let seed = matches.value_of("seed").map(|s| s.to_string());

    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);
    let (stake_authority, stake_authority_pubkey) =
        signer_of(matches, STAKE_AUTHORITY_ARG.name, wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;

    let mut bulk_signers = vec![stake_authority, fee_payer, split_stake_account];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::SplitStake {
            stake_account_pubkey,
            stake_authority: signer_info.index_of(stake_authority_pubkey).unwrap(),
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            split_stake_account: signer_info.index_of(split_stake_account_pubkey).unwrap(),
            seed,
            lamports,
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_merge_stake(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();

    let source_stake_account_pubkey = pubkey_of(matches, "source_stake_account_pubkey").unwrap();

    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);
    let (stake_authority, stake_authority_pubkey) =
        signer_of(matches, STAKE_AUTHORITY_ARG.name, wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;

    let mut bulk_signers = vec![stake_authority, fee_payer];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::MergeStake {
            stake_account_pubkey,
            source_stake_account_pubkey,
            stake_authority: signer_info.index_of(stake_authority_pubkey).unwrap(),
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_stake_deactivate_stake(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();
    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);
    let (stake_authority, stake_authority_pubkey) =
        signer_of(matches, STAKE_AUTHORITY_ARG.name, wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;

    let mut bulk_signers = vec![stake_authority, fee_payer];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::DeactivateStake {
            stake_account_pubkey,
            stake_authority: signer_info.index_of(stake_authority_pubkey).unwrap(),
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_stake_withdraw_stake(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();
    let destination_account_pubkey =
        pubkey_of_signer(matches, "destination_account_pubkey", wallet_manager)?.unwrap();
    let lamports = lamports_of_sol(matches, "amount").unwrap();
    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);
    let (withdraw_authority, withdraw_authority_pubkey) =
        signer_of(matches, WITHDRAW_AUTHORITY_ARG.name, wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;
    let (custodian, custodian_pubkey) = signer_of(matches, "custodian", wallet_manager)?;

    let mut bulk_signers = vec![withdraw_authority, fee_payer];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    if custodian.is_some() {
        bulk_signers.push(custodian);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::WithdrawStake {
            stake_account_pubkey,
            destination_account_pubkey,
            lamports,
            withdraw_authority: signer_info.index_of(withdraw_authority_pubkey).unwrap(),
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
            custodian: custodian_pubkey.and_then(|_| signer_info.index_of(custodian_pubkey)),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_stake_set_lockup(
    matches: &ArgMatches<'_>,
    default_signer_path: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();
    let epoch = value_of(matches, "lockup_epoch");
    let unix_timestamp = unix_timestamp_from_rfc3339_datetime(matches, "lockup_date");
    let new_custodian = pubkey_of_signer(matches, "new_custodian", wallet_manager)?;

    let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
    let blockhash_query = BlockhashQuery::new_from_matches(matches);
    let nonce_account = pubkey_of(matches, NONCE_ARG.name);

    let (custodian, custodian_pubkey) = signer_of(matches, "custodian", wallet_manager)?;
    let (nonce_authority, nonce_authority_pubkey) =
        signer_of(matches, NONCE_AUTHORITY_ARG.name, wallet_manager)?;
    let (fee_payer, fee_payer_pubkey) = signer_of(matches, FEE_PAYER_ARG.name, wallet_manager)?;

    let mut bulk_signers = vec![custodian, fee_payer];
    if nonce_account.is_some() {
        bulk_signers.push(nonce_authority);
    }
    let signer_info =
        generate_unique_signers(bulk_signers, matches, default_signer_path, wallet_manager)?;

    Ok(CliCommandInfo {
        command: CliCommand::StakeSetLockup {
            stake_account_pubkey,
            lockup: LockupArgs {
                custodian: new_custodian,
                epoch,
                unix_timestamp,
            },
            custodian: signer_info.index_of(custodian_pubkey).unwrap(),
            sign_only,
            blockhash_query,
            nonce_account,
            nonce_authority: signer_info.index_of(nonce_authority_pubkey).unwrap(),
            fee_payer: signer_info.index_of(fee_payer_pubkey).unwrap(),
        },
        signers: signer_info.signers,
    })
}

pub fn parse_show_stake_account(
    matches: &ArgMatches<'_>,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Result<CliCommandInfo, CliError> {
    let stake_account_pubkey =
        pubkey_of_signer(matches, "stake_account_pubkey", wallet_manager)?.unwrap();
    let use_lamports_unit = matches.is_present("lamports");
    Ok(CliCommandInfo {
        command: CliCommand::ShowStakeAccount {
            pubkey: stake_account_pubkey,
            use_lamports_unit,
        },
        signers: vec![],
    })
}

pub fn parse_show_stake_history(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, CliError> {
    let use_lamports_unit = matches.is_present("lamports");
    Ok(CliCommandInfo {
        command: CliCommand::ShowStakeHistory { use_lamports_unit },
        signers: vec![],
    })
}

#[allow(clippy::too_many_arguments)]
pub fn process_create_stake_account(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account: SignerIndex,
    seed: &Option<String>,
    staker: &Option<Pubkey>,
    withdrawer: &Option<Pubkey>,
    lockup: &Lockup,
    amount: SpendAmount,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<&Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
    from: SignerIndex,
) -> ProcessResult {
    let stake_account = config.signers[stake_account];
    let stake_account_address = if let Some(seed) = seed {
        Pubkey::create_with_seed(&stake_account.pubkey(), &seed, &solana_stake_program::id())?
    } else {
        stake_account.pubkey()
    };
    let from = config.signers[from];
    check_unique_pubkeys(
        (&from.pubkey(), "from keypair".to_string()),
        (&stake_account_address, "stake_account".to_string()),
    )?;

    let fee_payer = config.signers[fee_payer];
    let nonce_authority = config.signers[nonce_authority];

    let build_message = |lamports| {
        let authorized = Authorized {
            staker: staker.unwrap_or(from.pubkey()),
            withdrawer: withdrawer.unwrap_or(from.pubkey()),
        };

        let ixs = if let Some(seed) = seed {
            stake_instruction::create_account_with_seed(
                &from.pubkey(),          // from
                &stake_account_address,  // to
                &stake_account.pubkey(), // base
                seed,                    // seed
                &authorized,
                lockup,
                lamports,
            )
        } else {
            stake_instruction::create_account(
                &from.pubkey(),
                &stake_account.pubkey(),
                &authorized,
                lockup,
                lamports,
            )
        };
        if let Some(nonce_account) = &nonce_account {
            Message::new_with_nonce(
                ixs,
                Some(&fee_payer.pubkey()),
                nonce_account,
                &nonce_authority.pubkey(),
            )
        } else {
            Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
        }
    };

    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;

    let (message, lamports) = resolve_spend_tx_and_check_account_balances(
        rpc_client,
        sign_only,
        amount,
        &fee_calculator,
        &from.pubkey(),
        &fee_payer.pubkey(),
        build_message,
    )?;

    if !sign_only {
        if let Ok(stake_account) = rpc_client.get_account(&stake_account_address) {
            let err_msg = if stake_account.owner == solana_stake_program::id() {
                format!("Stake account {} already exists", stake_account_address)
            } else {
                format!(
                    "Account {} already exists and is not a stake account",
                    stake_account_address
                )
            };
            return Err(CliError::BadParameter(err_msg).into());
        }

        let minimum_balance =
            rpc_client.get_minimum_balance_for_rent_exemption(std::mem::size_of::<StakeState>())?;

        if lamports < minimum_balance {
            return Err(CliError::BadParameter(format!(
                "need at least {} lamports for stake account to be rent exempt, provided lamports: {}",
                minimum_balance, lamports
            ))
            .into());
        }

        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
    }

    let mut tx = Transaction::new_unsigned(message);
    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<SystemError>(result, &config)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_stake_authorize(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    new_authorizations: &[(StakeAuthorize, Pubkey, SignerIndex)],
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
) -> ProcessResult {
    let mut ixs = Vec::new();
    for (stake_authorize, authorized_pubkey, authority) in new_authorizations.iter() {
        check_unique_pubkeys(
            (stake_account_pubkey, "stake_account_pubkey".to_string()),
            (authorized_pubkey, "new_authorized_pubkey".to_string()),
        )?;
        let authority = config.signers[*authority];
        ixs.push(stake_instruction::authorize(
            stake_account_pubkey, // stake account to update
            &authority.pubkey(),  // currently authorized
            authorized_pubkey,    // new stake signer
            *stake_authorize,     // stake or withdraw
        ));
    }

    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;

    let nonce_authority = config.signers[nonce_authority];
    let fee_payer = config.signers[fee_payer];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<StakeError>(result, &config)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_deactivate_stake_account(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    stake_authority: SignerIndex,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
) -> ProcessResult {
    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;
    let stake_authority = config.signers[stake_authority];
    let ixs = vec![stake_instruction::deactivate_stake(
        stake_account_pubkey,
        &stake_authority.pubkey(),
    )];
    let nonce_authority = config.signers[nonce_authority];
    let fee_payer = config.signers[fee_payer];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<StakeError>(result, &config)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_withdraw_stake(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    destination_account_pubkey: &Pubkey,
    lamports: u64,
    withdraw_authority: SignerIndex,
    custodian: Option<SignerIndex>,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<&Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
) -> ProcessResult {
    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;
    let withdraw_authority = config.signers[withdraw_authority];
    let custodian = custodian.map(|index| config.signers[index]);

    let ixs = vec![stake_instruction::withdraw(
        stake_account_pubkey,
        &withdraw_authority.pubkey(),
        destination_account_pubkey,
        lamports,
        custodian.map(|signer| signer.pubkey()).as_ref(),
    )];

    let fee_payer = config.signers[fee_payer];
    let nonce_authority = config.signers[nonce_authority];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<SystemError>(result, &config)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_split_stake(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    stake_authority: SignerIndex,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: SignerIndex,
    split_stake_account: SignerIndex,
    split_stake_account_seed: &Option<String>,
    lamports: u64,
    fee_payer: SignerIndex,
) -> ProcessResult {
    let split_stake_account = config.signers[split_stake_account];
    let fee_payer = config.signers[fee_payer];

    if split_stake_account_seed.is_none() {
        check_unique_pubkeys(
            (&fee_payer.pubkey(), "fee-payer keypair".to_string()),
            (
                &split_stake_account.pubkey(),
                "split_stake_account".to_string(),
            ),
        )?;
    }
    check_unique_pubkeys(
        (&fee_payer.pubkey(), "fee-payer keypair".to_string()),
        (&stake_account_pubkey, "stake_account".to_string()),
    )?;
    check_unique_pubkeys(
        (&stake_account_pubkey, "stake_account".to_string()),
        (
            &split_stake_account.pubkey(),
            "split_stake_account".to_string(),
        ),
    )?;

    let stake_authority = config.signers[stake_authority];

    let split_stake_account_address = if let Some(seed) = split_stake_account_seed {
        Pubkey::create_with_seed(
            &split_stake_account.pubkey(),
            &seed,
            &solana_stake_program::id(),
        )?
    } else {
        split_stake_account.pubkey()
    };

    if !sign_only {
        if let Ok(stake_account) = rpc_client.get_account(&split_stake_account_address) {
            let err_msg = if stake_account.owner == solana_stake_program::id() {
                format!(
                    "Stake account {} already exists",
                    split_stake_account_address
                )
            } else {
                format!(
                    "Account {} already exists and is not a stake account",
                    split_stake_account_address
                )
            };
            return Err(CliError::BadParameter(err_msg).into());
        }

        let minimum_balance =
            rpc_client.get_minimum_balance_for_rent_exemption(std::mem::size_of::<StakeState>())?;

        if lamports < minimum_balance {
            return Err(CliError::BadParameter(format!(
                "need at least {} lamports for stake account to be rent exempt, provided lamports: {}",
                minimum_balance, lamports
            ))
            .into());
        }
    }

    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;

    let ixs = if let Some(seed) = split_stake_account_seed {
        stake_instruction::split_with_seed(
            &stake_account_pubkey,
            &stake_authority.pubkey(),
            lamports,
            &split_stake_account_address,
            &split_stake_account.pubkey(),
            seed,
        )
    } else {
        stake_instruction::split(
            &stake_account_pubkey,
            &stake_authority.pubkey(),
            lamports,
            &split_stake_account_address,
        )
    };

    let nonce_authority = config.signers[nonce_authority];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<StakeError>(result, &config)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_merge_stake(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    source_stake_account_pubkey: &Pubkey,
    stake_authority: SignerIndex,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
) -> ProcessResult {
    let fee_payer = config.signers[fee_payer];

    check_unique_pubkeys(
        (&fee_payer.pubkey(), "fee-payer keypair".to_string()),
        (&stake_account_pubkey, "stake_account".to_string()),
    )?;
    check_unique_pubkeys(
        (&fee_payer.pubkey(), "fee-payer keypair".to_string()),
        (
            &source_stake_account_pubkey,
            "source_stake_account".to_string(),
        ),
    )?;
    check_unique_pubkeys(
        (&stake_account_pubkey, "stake_account".to_string()),
        (
            &source_stake_account_pubkey,
            "source_stake_account".to_string(),
        ),
    )?;

    let stake_authority = config.signers[stake_authority];

    if !sign_only {
        for stake_account_address in &[stake_account_pubkey, source_stake_account_pubkey] {
            if let Ok(stake_account) = rpc_client.get_account(stake_account_address) {
                if stake_account.owner != solana_stake_program::id() {
                    return Err(CliError::BadParameter(format!(
                        "Account {} is not a stake account",
                        stake_account_address
                    ))
                    .into());
                }
            }
        }
    }

    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;

    let ixs = stake_instruction::merge(
        &stake_account_pubkey,
        &source_stake_account_pubkey,
        &stake_authority.pubkey(),
    );

    let nonce_authority = config.signers[nonce_authority];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<StakeError>(result, &config)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_stake_set_lockup(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    lockup: &mut LockupArgs,
    custodian: SignerIndex,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
) -> ProcessResult {
    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;
    let custodian = config.signers[custodian];

    let ixs = vec![stake_instruction::set_lockup(
        stake_account_pubkey,
        lockup,
        &custodian.pubkey(),
    )];
    let nonce_authority = config.signers[nonce_authority];
    let fee_payer = config.signers[fee_payer];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<StakeError>(result, &config)
    }
}

fn u64_some_if_not_zero(n: u64) -> Option<u64> {
    if n > 0 {
        Some(n)
    } else {
        None
    }
}

pub fn build_stake_state(
    account_balance: u64,
    stake_state: &StakeState,
    use_lamports_unit: bool,
    stake_history: &StakeHistory,
    clock: &Clock,
) -> CliStakeState {
    match stake_state {
        StakeState::Stake(
            Meta {
                rent_exempt_reserve,
                authorized,
                lockup,
            },
            stake,
        ) => {
            let current_epoch = clock.epoch;
            let (active_stake, activating_stake, deactivating_stake) = stake
                .delegation
                .stake_activating_and_deactivating(current_epoch, Some(stake_history));
            let lockup = if lockup.is_in_force(clock, &HashSet::new()) {
                Some(lockup.into())
            } else {
                None
            };
            CliStakeState {
                stake_type: CliStakeType::Stake,
                account_balance,
                delegated_stake: Some(stake.delegation.stake),
                delegated_vote_account_address: if stake.delegation.voter_pubkey
                    != Pubkey::default()
                {
                    Some(stake.delegation.voter_pubkey.to_string())
                } else {
                    None
                },
                activation_epoch: Some(if stake.delegation.activation_epoch < std::u64::MAX {
                    stake.delegation.activation_epoch
                } else {
                    0
                }),
                deactivation_epoch: if stake.delegation.deactivation_epoch < std::u64::MAX {
                    Some(stake.delegation.deactivation_epoch)
                } else {
                    None
                },
                authorized: Some(authorized.into()),
                lockup,
                use_lamports_unit,
                current_epoch,
                rent_exempt_reserve: Some(*rent_exempt_reserve),
                active_stake: u64_some_if_not_zero(active_stake),
                activating_stake: u64_some_if_not_zero(activating_stake),
                deactivating_stake: u64_some_if_not_zero(deactivating_stake),
            }
        }
        StakeState::RewardsPool => CliStakeState {
            stake_type: CliStakeType::RewardsPool,
            account_balance,
            ..CliStakeState::default()
        },
        StakeState::Uninitialized => CliStakeState {
            account_balance,
            ..CliStakeState::default()
        },
        StakeState::Initialized(Meta {
            rent_exempt_reserve,
            authorized,
            lockup,
        }) => {
            let lockup = if lockup.is_in_force(clock, &HashSet::new()) {
                Some(lockup.into())
            } else {
                None
            };
            CliStakeState {
                stake_type: CliStakeType::Initialized,
                account_balance,
                authorized: Some(authorized.into()),
                lockup,
                use_lamports_unit,
                rent_exempt_reserve: Some(*rent_exempt_reserve),
                ..CliStakeState::default()
            }
        }
    }
}

pub fn process_show_stake_account(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    use_lamports_unit: bool,
) -> ProcessResult {
    let stake_account = rpc_client.get_account(stake_account_pubkey)?;
    if stake_account.owner != solana_stake_program::id() {
        return Err(CliError::RpcRequestError(format!(
            "{:?} is not a stake account",
            stake_account_pubkey,
        ))
        .into());
    }
    match stake_account.state() {
        Ok(stake_state) => {
            let stake_history_account = rpc_client.get_account(&stake_history::id())?;
            let stake_history =
                StakeHistory::from_account(&stake_history_account).ok_or_else(|| {
                    CliError::RpcRequestError("Failed to deserialize stake history".to_string())
                })?;
            let clock_account = rpc_client.get_account(&clock::id())?;
            let clock: Clock = Sysvar::from_account(&clock_account).ok_or_else(|| {
                CliError::RpcRequestError("Failed to deserialize clock sysvar".to_string())
            })?;

            let state = build_stake_state(
                stake_account.lamports,
                &stake_state,
                use_lamports_unit,
                &stake_history,
                &clock,
            );
            Ok(config.output_format.formatted_string(&state))
        }
        Err(err) => Err(CliError::RpcRequestError(format!(
            "Account data could not be deserialized to stake state: {}",
            err
        ))
        .into()),
    }
}

pub fn process_show_stake_history(
    rpc_client: &RpcClient,
    config: &CliConfig,
    use_lamports_unit: bool,
) -> ProcessResult {
    let stake_history_account = rpc_client.get_account(&stake_history::id())?;
    let stake_history = StakeHistory::from_account(&stake_history_account).ok_or_else(|| {
        CliError::RpcRequestError("Failed to deserialize stake history".to_string())
    })?;

    let mut entries: Vec<CliStakeHistoryEntry> = vec![];
    for entry in stake_history.deref() {
        entries.push(entry.into());
    }
    let stake_history_output = CliStakeHistory {
        entries,
        use_lamports_unit,
    };
    Ok(config.output_format.formatted_string(&stake_history_output))
}

#[allow(clippy::too_many_arguments)]
pub fn process_delegate_stake(
    rpc_client: &RpcClient,
    config: &CliConfig,
    stake_account_pubkey: &Pubkey,
    vote_account_pubkey: &Pubkey,
    stake_authority: SignerIndex,
    force: bool,
    sign_only: bool,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: SignerIndex,
    fee_payer: SignerIndex,
) -> ProcessResult {
    check_unique_pubkeys(
        (&config.signers[0].pubkey(), "cli keypair".to_string()),
        (stake_account_pubkey, "stake_account_pubkey".to_string()),
    )?;
    let stake_authority = config.signers[stake_authority];

    if !sign_only {
        // Sanity check the vote account to ensure it is attached to a validator that has recently
        // voted at the tip of the ledger
        let vote_account_data = rpc_client
            .get_account_data(vote_account_pubkey)
            .map_err(|_| {
                CliError::RpcRequestError(format!(
                    "Vote account not found: {}",
                    vote_account_pubkey
                ))
            })?;

        let vote_state = VoteState::deserialize(&vote_account_data).map_err(|_| {
            CliError::RpcRequestError(
                "Account data could not be deserialized to vote state".to_string(),
            )
        })?;

        let sanity_check_result = match vote_state.root_slot {
            None => Err(CliError::BadParameter(
                "Unable to delegate. Vote account has no root slot".to_string(),
            )),
            Some(root_slot) => {
                let min_root_slot = rpc_client
                    .get_slot()?
                    .saturating_sub(DELINQUENT_VALIDATOR_SLOT_DISTANCE);
                if root_slot < min_root_slot {
                    Err(CliError::DynamicProgramError(format!(
                        "Unable to delegate.  Vote account appears delinquent \
                                 because its current root slot, {}, is less than {}",
                        root_slot, min_root_slot
                    )))
                } else {
                    Ok(())
                }
            }
        };

        if let Err(err) = &sanity_check_result {
            if !force {
                sanity_check_result?;
            } else {
                println!("--force supplied, ignoring: {}", err);
            }
        }
    }

    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_and_fee_calculator(rpc_client)?;

    let ixs = vec![stake_instruction::delegate_stake(
        stake_account_pubkey,
        &stake_authority.pubkey(),
        vote_account_pubkey,
    )];
    let nonce_authority = config.signers[nonce_authority];
    let fee_payer = config.signers[fee_payer];

    let message = if let Some(nonce_account) = &nonce_account {
        Message::new_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            nonce_account,
            &nonce_authority.pubkey(),
        )
    } else {
        Message::new_with_payer(&ixs, Some(&fee_payer.pubkey()))
    };
    let mut tx = Transaction::new_unsigned(message);

    if sign_only {
        tx.try_partial_sign(&config.signers, recent_blockhash)?;
        return_signers(&tx, &config)
    } else {
        tx.try_sign(&config.signers, recent_blockhash)?;
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority.pubkey(), &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction_with_spinner(&tx);
        log_instruction_custom_error::<StakeError>(result, &config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{app, parse_command};
    use solana_sdk::{
        hash::Hash,
        signature::{
            keypair_from_seed, read_keypair_file, write_keypair, Keypair, Presigner, Signer,
        },
    };
    use tempfile::NamedTempFile;

    fn make_tmp_file() -> (String, NamedTempFile) {
        let tmp_file = NamedTempFile::new().unwrap();
        (String::from(tmp_file.path().to_str().unwrap()), tmp_file)
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_parse_command() {
        let test_commands = app("test", "desc", "version");
        let default_keypair = Keypair::new();
        let (default_keypair_file, mut tmp_file) = make_tmp_file();
        write_keypair(&default_keypair, tmp_file.as_file_mut()).unwrap();
        let (keypair_file, mut tmp_file) = make_tmp_file();
        let stake_account_keypair = Keypair::new();
        write_keypair(&stake_account_keypair, tmp_file.as_file_mut()).unwrap();
        let stake_account_pubkey = stake_account_keypair.pubkey();
        let (stake_authority_keypair_file, mut tmp_file) = make_tmp_file();
        let stake_authority_keypair = Keypair::new();
        write_keypair(&stake_authority_keypair, tmp_file.as_file_mut()).unwrap();
        let (custodian_keypair_file, mut tmp_file) = make_tmp_file();
        let custodian_keypair = Keypair::new();
        write_keypair(&custodian_keypair, tmp_file.as_file_mut()).unwrap();

        // stake-authorize subcommand
        let stake_account_string = stake_account_pubkey.to_string();
        let new_stake_authority = Pubkey::new(&[1u8; 32]);
        let new_stake_string = new_stake_authority.to_string();
        let new_withdraw_authority = Pubkey::new(&[2u8; 32]);
        let new_withdraw_string = new_withdraw_authority.to_string();
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &new_stake_string,
            "--new-withdraw-authority",
            &new_withdraw_string,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![
                        (StakeAuthorize::Staker, new_stake_authority, 0,),
                        (StakeAuthorize::Withdrawer, new_withdraw_authority, 0,),
                    ],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into(),],
            },
        );
        let (withdraw_authority_keypair_file, mut tmp_file) = make_tmp_file();
        let withdraw_authority_keypair = Keypair::new();
        write_keypair(&withdraw_authority_keypair, tmp_file.as_file_mut()).unwrap();
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &new_stake_string,
            "--new-withdraw-authority",
            &new_withdraw_string,
            "--stake-authority",
            &stake_authority_keypair_file,
            "--withdraw-authority",
            &withdraw_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![
                        (StakeAuthorize::Staker, new_stake_authority, 1,),
                        (StakeAuthorize::Withdrawer, new_withdraw_authority, 2,),
                    ],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&stake_authority_keypair_file)
                        .unwrap()
                        .into(),
                    read_keypair_file(&withdraw_authority_keypair_file)
                        .unwrap()
                        .into(),
                ],
            },
        );
        // Withdraw authority may set both new authorities
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &new_stake_string,
            "--new-withdraw-authority",
            &new_withdraw_string,
            "--withdraw-authority",
            &withdraw_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![
                        (StakeAuthorize::Staker, new_stake_authority, 1,),
                        (StakeAuthorize::Withdrawer, new_withdraw_authority, 1,),
                    ],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&withdraw_authority_keypair_file)
                        .unwrap()
                        .into(),
                ],
            },
        );
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &new_stake_string,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, new_stake_authority, 0,),],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into(),],
            },
        );
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &new_stake_string,
            "--stake-authority",
            &stake_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, new_stake_authority, 1,),],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&stake_authority_keypair_file)
                        .unwrap()
                        .into(),
                ],
            },
        );
        // Withdraw authority may set new stake authority
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &new_stake_string,
            "--withdraw-authority",
            &withdraw_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, new_stake_authority, 1,),],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&withdraw_authority_keypair_file)
                        .unwrap()
                        .into(),
                ],
            },
        );
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-withdraw-authority",
            &new_withdraw_string,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(
                        StakeAuthorize::Withdrawer,
                        new_withdraw_authority,
                        0,
                    ),],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into(),],
            },
        );
        let test_stake_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-withdraw-authority",
            &new_withdraw_string,
            "--withdraw-authority",
            &withdraw_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_stake_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(
                        StakeAuthorize::Withdrawer,
                        new_withdraw_authority,
                        1,
                    ),],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&withdraw_authority_keypair_file)
                        .unwrap()
                        .into(),
                ],
            },
        );

        // Test Authorize Subcommand w/ sign-only
        let blockhash = Hash::default();
        let blockhash_string = format!("{}", blockhash);
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--sign-only",
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: true,
                    blockhash_query: BlockhashQuery::None(blockhash),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );
        // Test Authorize Subcommand w/ offline feepayer
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        let sig = keypair.sign_message(&[0u8]);
        let signer = format!("{}={}", keypair.pubkey(), sig);
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer,
            "--fee-payer",
            &pubkey.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&pubkey, &sig).into()
                ],
            }
        );
        // Test Authorize Subcommand w/ offline fee payer and nonce authority
        let keypair2 = Keypair::new();
        let pubkey2 = keypair2.pubkey();
        let sig2 = keypair.sign_message(&[0u8]);
        let signer2 = format!("{}={}", keypair2.pubkey(), sig2);
        let nonce_account = Pubkey::new(&[1u8; 32]);
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer,
            "--signer",
            &signer2,
            "--fee-payer",
            &pubkey.to_string(),
            "--nonce",
            &nonce_account.to_string(),
            "--nonce-authority",
            &pubkey2.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account),
                        blockhash
                    ),
                    nonce_account: Some(nonce_account),
                    nonce_authority: 2,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&pubkey, &sig).into(),
                    Presigner::new(&pubkey2, &sig2).into(),
                ],
            }
        );
        // Test Authorize Subcommand w/ blockhash
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );
        // Test Authorize Subcommand w/ nonce
        let (nonce_keypair_file, mut nonce_tmp_file) = make_tmp_file();
        let nonce_authority_keypair = Keypair::new();
        write_keypair(&nonce_authority_keypair, nonce_tmp_file.as_file_mut()).unwrap();
        let nonce_account_pubkey = nonce_authority_keypair.pubkey();
        let nonce_account_string = nonce_account_pubkey.to_string();
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--nonce",
            &nonce_account_string,
            "--nonce-authority",
            &nonce_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account_pubkey),
                        blockhash
                    ),
                    nonce_account: Some(nonce_account_pubkey),
                    nonce_authority: 1,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    nonce_authority_keypair.into()
                ],
            }
        );
        // Test Authorize Subcommand w/ fee-payer
        let (fee_payer_keypair_file, mut fee_payer_tmp_file) = make_tmp_file();
        let fee_payer_keypair = Keypair::new();
        write_keypair(&fee_payer_keypair, fee_payer_tmp_file.as_file_mut()).unwrap();
        let fee_payer_pubkey = fee_payer_keypair.pubkey();
        let fee_payer_string = fee_payer_pubkey.to_string();
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--fee-payer",
            &fee_payer_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&fee_payer_keypair_file).unwrap().into(),
                ],
            }
        );
        // Test Authorize Subcommand w/ absentee fee-payer
        let sig = fee_payer_keypair.sign_message(&[0u8]);
        let signer = format!("{}={}", fee_payer_string, sig);
        let test_authorize = test_commands.clone().get_matches_from(vec![
            "test",
            "stake-authorize",
            &stake_account_string,
            "--new-stake-authority",
            &stake_account_string,
            "--fee-payer",
            &fee_payer_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer,
        ]);
        assert_eq!(
            parse_command(&test_authorize, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::StakeAuthorize {
                    stake_account_pubkey,
                    new_authorizations: vec![(StakeAuthorize::Staker, stake_account_pubkey, 0)],
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&fee_payer_pubkey, &sig).into()
                ],
            }
        );

        // Test CreateStakeAccount SubCommand
        let custodian = Pubkey::new_rand();
        let custodian_string = format!("{}", custodian);
        let authorized = Pubkey::new_rand();
        let authorized_string = format!("{}", authorized);
        let test_create_stake_account = test_commands.clone().get_matches_from(vec![
            "test",
            "create-stake-account",
            &keypair_file,
            "50",
            "--stake-authority",
            &authorized_string,
            "--withdraw-authority",
            &authorized_string,
            "--custodian",
            &custodian_string,
            "--lockup-epoch",
            "43",
        ]);
        assert_eq!(
            parse_command(&test_create_stake_account, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::CreateStakeAccount {
                    stake_account: 1,
                    seed: None,
                    staker: Some(authorized),
                    withdrawer: Some(authorized),
                    lockup: Lockup {
                        epoch: 43,
                        unix_timestamp: 0,
                        custodian,
                    },
                    amount: SpendAmount::Some(50_000_000_000),
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                    from: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    stake_account_keypair.into()
                ],
            }
        );

        let (keypair_file, mut tmp_file) = make_tmp_file();
        let stake_account_keypair = Keypair::new();
        write_keypair(&stake_account_keypair, tmp_file.as_file_mut()).unwrap();
        let stake_account_pubkey = stake_account_keypair.pubkey();
        let stake_account_string = stake_account_pubkey.to_string();

        let test_create_stake_account2 = test_commands.clone().get_matches_from(vec![
            "test",
            "create-stake-account",
            &keypair_file,
            "50",
        ]);

        assert_eq!(
            parse_command(
                &test_create_stake_account2,
                &default_keypair_file,
                &mut None
            )
            .unwrap(),
            CliCommandInfo {
                command: CliCommand::CreateStakeAccount {
                    stake_account: 1,
                    seed: None,
                    staker: None,
                    withdrawer: None,
                    lockup: Lockup::default(),
                    amount: SpendAmount::Some(50_000_000_000),
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                    from: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&keypair_file).unwrap().into()
                ],
            }
        );

        // CreateStakeAccount offline and nonce
        let nonce_account = Pubkey::new(&[1u8; 32]);
        let nonce_account_string = nonce_account.to_string();
        let offline = keypair_from_seed(&[2u8; 32]).unwrap();
        let offline_pubkey = offline.pubkey();
        let offline_string = offline_pubkey.to_string();
        let offline_sig = offline.sign_message(&[3u8]);
        let offline_signer = format!("{}={}", offline_pubkey, offline_sig);
        let nonce_hash = Hash::new(&[4u8; 32]);
        let nonce_hash_string = nonce_hash.to_string();
        let test_create_stake_account2 = test_commands.clone().get_matches_from(vec![
            "test",
            "create-stake-account",
            &keypair_file,
            "50",
            "--blockhash",
            &nonce_hash_string,
            "--nonce",
            &nonce_account_string,
            "--nonce-authority",
            &offline_string,
            "--fee-payer",
            &offline_string,
            "--from",
            &offline_string,
            "--signer",
            &offline_signer,
        ]);

        assert_eq!(
            parse_command(
                &test_create_stake_account2,
                &default_keypair_file,
                &mut None
            )
            .unwrap(),
            CliCommandInfo {
                command: CliCommand::CreateStakeAccount {
                    stake_account: 1,
                    seed: None,
                    staker: None,
                    withdrawer: None,
                    lockup: Lockup::default(),
                    amount: SpendAmount::Some(50_000_000_000),
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account),
                        nonce_hash
                    ),
                    nonce_account: Some(nonce_account),
                    nonce_authority: 0,
                    fee_payer: 0,
                    from: 0,
                },
                signers: vec![
                    Presigner::new(&offline_pubkey, &offline_sig).into(),
                    read_keypair_file(&keypair_file).unwrap().into()
                ],
            }
        );

        // Test DelegateStake Subcommand
        let vote_account_pubkey = Pubkey::new_rand();
        let vote_account_string = vote_account_pubkey.to_string();
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: false,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        // Test DelegateStake Subcommand w/ authority
        let vote_account_pubkey = Pubkey::new_rand();
        let vote_account_string = vote_account_pubkey.to_string();
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
            "--stake-authority",
            &stake_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 1,
                    force: false,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&stake_authority_keypair_file)
                        .unwrap()
                        .into()
                ],
            }
        );

        // Test DelegateStake Subcommand w/ force
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            "--force",
            &stake_account_string,
            &vote_account_string,
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: true,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        // Test Delegate Subcommand w/ Blockhash
        let blockhash = Hash::default();
        let blockhash_string = format!("{}", blockhash);
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
            "--blockhash",
            &blockhash_string,
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: false,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
            "--blockhash",
            &blockhash_string,
            "--sign-only",
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: false,
                    sign_only: true,
                    blockhash_query: BlockhashQuery::None(blockhash),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        // Test Delegate Subcommand w/ absent fee payer
        let key1 = Pubkey::new_rand();
        let sig1 = Keypair::new().sign_message(&[0u8]);
        let signer1 = format!("{}={}", key1, sig1);
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer1,
            "--fee-payer",
            &key1.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: false,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&key1, &sig1).into()
                ],
            }
        );

        // Test Delegate Subcommand w/ absent fee payer and absent nonce authority
        let key2 = Pubkey::new_rand();
        let sig2 = Keypair::new().sign_message(&[0u8]);
        let signer2 = format!("{}={}", key2, sig2);
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer1,
            "--signer",
            &signer2,
            "--fee-payer",
            &key1.to_string(),
            "--nonce",
            &nonce_account.to_string(),
            "--nonce-authority",
            &key2.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: false,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account),
                        blockhash
                    ),
                    nonce_account: Some(nonce_account),
                    nonce_authority: 2,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&key1, &sig1).into(),
                    Presigner::new(&key2, &sig2).into(),
                ],
            }
        );

        // Test Delegate Subcommand w/ present fee-payer
        let (fee_payer_keypair_file, mut fee_payer_tmp_file) = make_tmp_file();
        let fee_payer_keypair = Keypair::new();
        write_keypair(&fee_payer_keypair, fee_payer_tmp_file.as_file_mut()).unwrap();
        let test_delegate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "delegate-stake",
            &stake_account_string,
            &vote_account_string,
            "--fee-payer",
            &fee_payer_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_delegate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DelegateStake {
                    stake_account_pubkey,
                    vote_account_pubkey,
                    stake_authority: 0,
                    force: false,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&fee_payer_keypair_file).unwrap().into()
                ],
            }
        );

        // Test WithdrawStake Subcommand
        let test_withdraw_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "withdraw-stake",
            &stake_account_string,
            &stake_account_string,
            "42",
        ]);

        assert_eq!(
            parse_command(&test_withdraw_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::WithdrawStake {
                    stake_account_pubkey,
                    destination_account_pubkey: stake_account_pubkey,
                    lamports: 42_000_000_000,
                    withdraw_authority: 0,
                    custodian: None,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        // Test WithdrawStake Subcommand w/ authority
        let test_withdraw_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "withdraw-stake",
            &stake_account_string,
            &stake_account_string,
            "42",
            "--withdraw-authority",
            &stake_authority_keypair_file,
        ]);

        assert_eq!(
            parse_command(&test_withdraw_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::WithdrawStake {
                    stake_account_pubkey,
                    destination_account_pubkey: stake_account_pubkey,
                    lamports: 42_000_000_000,
                    withdraw_authority: 1,
                    custodian: None,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&stake_authority_keypair_file)
                        .unwrap()
                        .into()
                ],
            }
        );

        // Test WithdrawStake Subcommand w/ custodian
        let test_withdraw_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "withdraw-stake",
            &stake_account_string,
            &stake_account_string,
            "42",
            "--custodian",
            &custodian_keypair_file,
        ]);

        assert_eq!(
            parse_command(&test_withdraw_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::WithdrawStake {
                    stake_account_pubkey,
                    destination_account_pubkey: stake_account_pubkey,
                    lamports: 42_000_000_000,
                    withdraw_authority: 0,
                    custodian: Some(1),
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&custodian_keypair_file).unwrap().into()
                ],
            }
        );

        // Test WithdrawStake Subcommand w/ authority and offline nonce
        let test_withdraw_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "withdraw-stake",
            &stake_account_string,
            &stake_account_string,
            "42",
            "--withdraw-authority",
            &stake_authority_keypair_file,
            "--blockhash",
            &nonce_hash_string,
            "--nonce",
            &nonce_account_string,
            "--nonce-authority",
            &offline_string,
            "--fee-payer",
            &offline_string,
            "--signer",
            &offline_signer,
        ]);

        assert_eq!(
            parse_command(&test_withdraw_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::WithdrawStake {
                    stake_account_pubkey,
                    destination_account_pubkey: stake_account_pubkey,
                    lamports: 42_000_000_000,
                    withdraw_authority: 0,
                    custodian: None,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account),
                        nonce_hash
                    ),
                    nonce_account: Some(nonce_account),
                    nonce_authority: 1,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&stake_authority_keypair_file)
                        .unwrap()
                        .into(),
                    Presigner::new(&offline_pubkey, &offline_sig).into()
                ],
            }
        );

        // Test DeactivateStake Subcommand
        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        // Test DeactivateStake Subcommand w/ authority
        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
            "--stake-authority",
            &stake_authority_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 1,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&stake_authority_keypair_file)
                        .unwrap()
                        .into()
                ],
            }
        );

        // Test Deactivate Subcommand w/ Blockhash
        let blockhash = Hash::default();
        let blockhash_string = format!("{}", blockhash);
        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--sign-only",
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: true,
                    blockhash_query: BlockhashQuery::None(blockhash),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into()],
            }
        );

        // Test Deactivate Subcommand w/ absent fee payer
        let key1 = Pubkey::new_rand();
        let sig1 = Keypair::new().sign_message(&[0u8]);
        let signer1 = format!("{}={}", key1, sig1);
        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer1,
            "--fee-payer",
            &key1.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::Cluster,
                        blockhash
                    ),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&key1, &sig1).into()
                ],
            }
        );

        // Test Deactivate Subcommand w/ absent fee payer and nonce authority
        let key2 = Pubkey::new_rand();
        let sig2 = Keypair::new().sign_message(&[0u8]);
        let signer2 = format!("{}={}", key2, sig2);
        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer1,
            "--signer",
            &signer2,
            "--fee-payer",
            &key1.to_string(),
            "--nonce",
            &nonce_account.to_string(),
            "--nonce-authority",
            &key2.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account),
                        blockhash
                    ),
                    nonce_account: Some(nonce_account),
                    nonce_authority: 2,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    Presigner::new(&key1, &sig1).into(),
                    Presigner::new(&key2, &sig2).into(),
                ],
            }
        );

        // Test Deactivate Subcommand w/ fee-payer
        let test_deactivate_stake = test_commands.clone().get_matches_from(vec![
            "test",
            "deactivate-stake",
            &stake_account_string,
            "--fee-payer",
            &fee_payer_keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_deactivate_stake, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::DeactivateStake {
                    stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::All(blockhash_query::Source::Cluster),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 1,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&fee_payer_keypair_file).unwrap().into()
                ],
            }
        );

        // Test SplitStake SubCommand
        let (keypair_file, mut tmp_file) = make_tmp_file();
        let stake_account_keypair = Keypair::new();
        write_keypair(&stake_account_keypair, tmp_file.as_file_mut()).unwrap();
        let (split_stake_account_keypair_file, mut tmp_file) = make_tmp_file();
        let split_stake_account_keypair = Keypair::new();
        write_keypair(&split_stake_account_keypair, tmp_file.as_file_mut()).unwrap();

        let test_split_stake_account = test_commands.clone().get_matches_from(vec![
            "test",
            "split-stake",
            &keypair_file,
            &split_stake_account_keypair_file,
            "50",
        ]);
        assert_eq!(
            parse_command(&test_split_stake_account, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::SplitStake {
                    stake_account_pubkey: stake_account_keypair.pubkey(),
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    split_stake_account: 1,
                    seed: None,
                    lamports: 50_000_000_000,
                    fee_payer: 0,
                },
                signers: vec![
                    read_keypair_file(&default_keypair_file).unwrap().into(),
                    read_keypair_file(&split_stake_account_keypair_file)
                        .unwrap()
                        .into()
                ],
            }
        );

        // Split stake offline nonced submission
        let nonce_account = Pubkey::new(&[1u8; 32]);
        let nonce_account_string = nonce_account.to_string();
        let nonce_auth = keypair_from_seed(&[2u8; 32]).unwrap();
        let nonce_auth_pubkey = nonce_auth.pubkey();
        let nonce_auth_string = nonce_auth_pubkey.to_string();
        let nonce_sig = nonce_auth.sign_message(&[0u8]);
        let nonce_signer = format!("{}={}", nonce_auth_pubkey, nonce_sig);
        let stake_auth = keypair_from_seed(&[3u8; 32]).unwrap();
        let stake_auth_pubkey = stake_auth.pubkey();
        let stake_auth_string = stake_auth_pubkey.to_string();
        let stake_sig = stake_auth.sign_message(&[0u8]);
        let stake_signer = format!("{}={}", stake_auth_pubkey, stake_sig);
        let nonce_hash = Hash::new(&[4u8; 32]);
        let nonce_hash_string = nonce_hash.to_string();

        let test_split_stake_account = test_commands.clone().get_matches_from(vec![
            "test",
            "split-stake",
            &keypair_file,
            &split_stake_account_keypair_file,
            "50",
            "--stake-authority",
            &stake_auth_string,
            "--blockhash",
            &nonce_hash_string,
            "--nonce",
            &nonce_account_string,
            "--nonce-authority",
            &nonce_auth_string,
            "--fee-payer",
            &nonce_auth_string, // Arbitrary choice of fee-payer
            "--signer",
            &nonce_signer,
            "--signer",
            &stake_signer,
        ]);
        assert_eq!(
            parse_command(&test_split_stake_account, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::SplitStake {
                    stake_account_pubkey: stake_account_keypair.pubkey(),
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::FeeCalculator(
                        blockhash_query::Source::NonceAccount(nonce_account),
                        nonce_hash
                    ),
                    nonce_account: Some(nonce_account),
                    nonce_authority: 1,
                    split_stake_account: 2,
                    seed: None,
                    lamports: 50_000_000_000,
                    fee_payer: 1,
                },
                signers: vec![
                    Presigner::new(&stake_auth_pubkey, &stake_sig).into(),
                    Presigner::new(&nonce_auth_pubkey, &nonce_sig).into(),
                    read_keypair_file(&split_stake_account_keypair_file)
                        .unwrap()
                        .into(),
                ],
            }
        );

        // Test MergeStake SubCommand
        let (keypair_file, mut tmp_file) = make_tmp_file();
        let stake_account_keypair = Keypair::new();
        write_keypair(&stake_account_keypair, tmp_file.as_file_mut()).unwrap();

        let source_stake_account_pubkey = Pubkey::new_rand();
        let test_merge_stake_account = test_commands.clone().get_matches_from(vec![
            "test",
            "merge-stake",
            &keypair_file,
            &source_stake_account_pubkey.to_string(),
        ]);
        assert_eq!(
            parse_command(&test_merge_stake_account, &default_keypair_file, &mut None).unwrap(),
            CliCommandInfo {
                command: CliCommand::MergeStake {
                    stake_account_pubkey: stake_account_keypair.pubkey(),
                    source_stake_account_pubkey,
                    stake_authority: 0,
                    sign_only: false,
                    blockhash_query: BlockhashQuery::default(),
                    nonce_account: None,
                    nonce_authority: 0,
                    fee_payer: 0,
                },
                signers: vec![read_keypair_file(&default_keypair_file).unwrap().into(),],
            }
        );
    }
}