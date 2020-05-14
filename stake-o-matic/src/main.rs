use clap::{crate_description, crate_name, crate_version, value_t, value_t_or_exit, App, Arg};
use log::*;
use solana_clap_utils::{
    input_parsers::{keypair_of, pubkey_of},
    input_validators::{is_keypair, is_pubkey_or_keypair, is_url},
};
use solana_client::{rpc_client::RpcClient, rpc_response::RpcVoteAccountInfo};
use solana_metrics::datapoint_info;
use solana_sdk::{
    account_utils::StateMut,
    clock::Slot,
    message::Message,
    native_token::*,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solana_stake_program::{stake_instruction, stake_state::StakeState};

use std::{
    collections::HashSet, error, fs::File, iter::FromIterator, path::PathBuf, process,
    str::FromStr, thread::sleep, time::Duration,
};

struct Config {
    json_rpc_url: String,
    metrics_cluster_name: String,
    source_stake_address: Pubkey,
    authorized_staker: Keypair,

    /// Only validators with an identity pubkey in this whitelist will be staked
    whitelist: HashSet<Pubkey>,

    dry_run: bool,

    /// Amount of lamports to stake any validator in the whitelist that is not delinquent
    baseline_stake_amount: u64,

    /// Amount of additional lamports to stake quality block producers in the whitelist
    bonus_stake_amount: u64,

    /// Quality validators produce a block in more than this percentage of their leader slots
    quality_block_producer_percentage: usize,

    /// A delinquent validator gets this number of slots of grace (from the current slot) before it
    /// will be fully destaked.  The grace period is intended to account for unexpected bugs that
    /// cause a validator to go down
    delinquent_grace_slot_distance: u64,
}

fn get_config() -> Config {
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(&config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url)
                .help("JSON RPC URL for the cluster"),
        )
        .arg(
            Arg::with_name("authorized_staker")
                .long("authorized-staker")
                .value_name("KEYPAIR")
                .validator(is_keypair)
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("source_stake_address")
                .long("source-stake-address")
                .value_name("ADDRESS")
                .takes_value(true)
                .required(true)
                .validator(is_pubkey_or_keypair)
                .help("The source stake account for splitting individual validator stake accounts from"),
        )
        .arg(
            Arg::with_name("whitelist_file")
                .long("whitelist")
                .value_name("FILE")
                .required(true)
                .takes_value(true)
                .help("File containing an YAML array of validator pubkeys eligible for staking"),
        )
        .arg(
            Arg::with_name("confirm")
                .long("confirm")
                .takes_value(false)
                .help("Confirm that the stake adjustments should actually be made"),
        )
        .get_matches();

    let config = if let Some(config_file) = matches.value_of("config_file") {
        solana_cli_config::Config::load(config_file).unwrap_or_default()
    } else {
        solana_cli_config::Config::default()
    };

    let json_rpc_url =
        value_t!(matches, "json_rpc_url", String).unwrap_or_else(|_| config.json_rpc_url);
    let source_stake_address = pubkey_of(&matches, "source_stake_address").unwrap();
    let authorized_staker = keypair_of(&matches, "authorized_staker").unwrap();
    let dry_run = !matches.is_present("confirm");

    let whitelist_file = File::open(value_t_or_exit!(matches, "whitelist_file", PathBuf))
        .unwrap_or_else(|err| {
            error!("Unable to open whitelist: {}", err);
            process::exit(1);
        });

    let whitelist = serde_yaml::from_reader::<_, Vec<String>>(whitelist_file)
        .unwrap_or_else(|err| {
            error!("Unable to read whitelist: {}", err);
            process::exit(1);
        })
        .into_iter()
        .map(|p| {
            Pubkey::from_str(&p).unwrap_or_else(|err| {
                error!("Invalid whitelist pubkey '{}': {}", p, err);
                process::exit(1);
            })
        })
        .collect::<HashSet<_>>();

    let metrics_cluster_name = if json_rpc_url.contains("mainnet-beta.solana.com") {
        "mainnet-beta"
    } else if json_rpc_url.contains("testnet.solana.com") {
        "testnet"
    } else if json_rpc_url.contains("devnet.solana.com") {
        "devnet"
    } else {
        "unknown"
    }
    .to_string();

    let config = Config {
        json_rpc_url,
        metrics_cluster_name,
        source_stake_address,
        authorized_staker,
        whitelist,
        dry_run,
        baseline_stake_amount: sol_to_lamports(5000.),
        bonus_stake_amount: sol_to_lamports(50_000.),
        delinquent_grace_slot_distance: 21600, // ~24 hours worth of slots at 2.5 slots per second
        quality_block_producer_percentage: 75,
    };

    info!("RPC URL: {}", config.json_rpc_url);
    config
}

fn get_stake_account(
    rpc_client: &RpcClient,
    address: &Pubkey,
) -> Result<(u64, StakeState), String> {
    let account = rpc_client.get_account(address).map_err(|e| {
        format!(
            "Failed to fetch stake account {}: {}",
            address,
            e.to_string()
        )
    })?;

    if account.owner != solana_stake_program::id() {
        return Err(format!(
            "not a stake account (owned by {}): {}",
            account.owner, address
        ));
    }

    account
        .state()
        .map_err(|e| {
            format!(
                "Failed to decode stake account at {}: {}",
                address,
                e.to_string()
            )
        })
        .map(|stake_state| (account.lamports, stake_state))
}

fn classify_block_producers(
    rpc_client: &RpcClient,
    config: &Config,
    first_slot_in_epoch: Slot,
    last_slot_in_epoch: Slot,
) -> Result<(HashSet<Pubkey>, HashSet<Pubkey>), Box<dyn error::Error>> {
    let minimum_ledger_slot = rpc_client.minimum_ledger_slot()?;
    if minimum_ledger_slot >= last_slot_in_epoch {
        return Err(format!(
            "Minimum ledger slot is newer than the last epoch: {} > {}",
            minimum_ledger_slot, last_slot_in_epoch
        )
        .into());
    }

    let first_slot = if minimum_ledger_slot > first_slot_in_epoch {
        minimum_ledger_slot
    } else {
        first_slot_in_epoch
    };

    let confirmed_blocks = rpc_client.get_confirmed_blocks(first_slot, Some(last_slot_in_epoch))?;
    let confirmed_blocks: HashSet<Slot> = HashSet::from_iter(confirmed_blocks.into_iter());

    let mut poor_block_producers = HashSet::new();
    let mut quality_block_producers = HashSet::new();

    let leader_schedule = rpc_client.get_leader_schedule(Some(first_slot))?.unwrap();
    for (validator_identity, relative_slots) in leader_schedule {
        let mut validator_blocks = 0;
        let mut validator_slots = 0;
        for relative_slot in relative_slots {
            let slot = first_slot_in_epoch + relative_slot as Slot;
            if slot >= first_slot {
                validator_slots += 1;
                if confirmed_blocks.contains(&slot) {
                    validator_blocks += 1;
                }
            }
        }
        trace!(
            "Validator {} produced {} blocks in {} slots",
            validator_identity,
            validator_blocks,
            validator_slots
        );
        if validator_slots > 0 {
            let validator_identity = Pubkey::from_str(&validator_identity)?;
            if validator_blocks * 100 / validator_slots > config.quality_block_producer_percentage {
                quality_block_producers.insert(validator_identity);
            } else {
                poor_block_producers.insert(validator_identity);
            }
        }
    }

    Ok((quality_block_producers, poor_block_producers))
}

fn transact(
    rpc_client: &RpcClient,
    dry_run: bool,
    transactions: Vec<(Transaction, String)>,
    authorized_staker: &Keypair,
) -> Result<Vec<(bool, String)>, Box<dyn error::Error>> {
    let (blockhash, fee_calculator) = rpc_client.get_recent_blockhash()?;

    let authorized_staker_balance = rpc_client.get_balance(&authorized_staker.pubkey())?;
    info!(
        "Authorized staker balance: {} SOL",
        lamports_to_sol(authorized_staker_balance)
    );

    let required_fee = transactions.iter().fold(0, |fee, (transaction, _)| {
        fee + fee_calculator.calculate_fee(&transaction.message)
    });
    info!("Required fee: {} SOL", lamports_to_sol(required_fee));
    if required_fee > authorized_staker_balance {
        return Err("Authorized staker has insufficient funds".into());
    }

    info!("{} transactions to send", transactions.len());

    if dry_run && !transactions.is_empty() {
        return Err("--confirm flag not provided, exiting before sending transactions".into());
    }

    let mut pending_transactions = vec![];
    let mut finalized_transactions = vec![];

    for (mut transaction, memo) in transactions.into_iter() {
        transaction.sign(&[authorized_staker], blockhash);
        info!("Sending transaction: {}", transaction.signatures[0]);
        match rpc_client.send_transaction(&transaction) {
            Ok(signature) => pending_transactions.push((signature, memo)),
            Err(err) => {
                error!("Failed to send transaction: {}", err);
                finalized_transactions.push((false, memo));
            }
        }
    }

    loop {
        if pending_transactions.is_empty() {
            break;
        }
        info!(
            "{} pending transactions, {} finalized transactions",
            pending_transactions.len(),
            finalized_transactions.len()
        );
        sleep(Duration::from_millis(2000));

        if rpc_client
            .get_fee_calculator_for_blockhash(&blockhash)?
            .is_none()
        {
            error!("Blockhash {} expired", blockhash);
            for (_signature, memo) in pending_transactions.into_iter() {
                finalized_transactions.push((false, memo));
            }
            break;
        }

        let statuses = rpc_client
            .get_signature_statuses(
                &pending_transactions
                    .iter()
                    .map(|(signature, _memo)| *signature)
                    .collect::<Vec<_>>(),
            )?
            .value;

        let mut still_pending_transactions = vec![];
        for ((signature, memo), status) in
            pending_transactions.into_iter().zip(statuses.into_iter())
        {
            trace!("{} - {:?}", signature, status);
            if let Some(status) = status {
                if status.confirmations.is_none() {
                    finalized_transactions.push((status.err.is_none(), memo));
                    continue;
                }
            }
            still_pending_transactions.push((signature, memo));
        }
        pending_transactions = still_pending_transactions;
    }

    Ok(finalized_transactions)
}

#[allow(clippy::cognitive_complexity)] // Yeah I know...
fn main() -> Result<(), Box<dyn error::Error>> {
    solana_logger::setup_with_default("solana=info");
    let config = get_config();

    let rpc_client = RpcClient::new(config.json_rpc_url.clone());
    let epoch_info = rpc_client.get_epoch_info()?;
    let last_epoch = epoch_info.epoch - 1;

    info!("Epoch info: {:?}", epoch_info);

    // check source stake account
    let (source_stake_balance, source_stake_state) =
        get_stake_account(&rpc_client, &config.source_stake_address)?;

    info!(
        "stake account balance: {} SOL",
        lamports_to_sol(source_stake_balance)
    );
    match &source_stake_state {
        StakeState::Initialized(_) => (),
        _ => {
            error!(
                "Source stake account is not in the initialized state: {:?}",
                source_stake_state
            );
            process::exit(1);
        }
    }

    let epoch_schedule = rpc_client.get_epoch_schedule()?;
    let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(last_epoch);
    let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(last_epoch);

    info!(
        "last epoch {}: slots {} to {}",
        last_epoch, first_slot_in_epoch, last_slot_in_epoch
    );

    let (quality_block_producers, poor_block_producers) = classify_block_producers(
        &rpc_client,
        &config,
        first_slot_in_epoch,
        last_slot_in_epoch,
    )?;
    trace!("quality_block_producers: {:?}", quality_block_producers);
    trace!("poor_block_producers: {:?}", poor_block_producers);

    // Fetch vote account status for all the whitelisted validators
    let vote_account_status = rpc_client.get_vote_accounts()?;
    let vote_account_info = vote_account_status
        .current
        .into_iter()
        .chain(vote_account_status.delinquent.into_iter())
        .filter_map(|vai| {
            let node_pubkey = Pubkey::from_str(&vai.node_pubkey).ok()?;
            if config.whitelist.contains(&node_pubkey) {
                Some(vai)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let mut source_stake_lamports_required = 0;
    let mut create_stake_transactions = vec![];
    let mut delegate_stake_transactions = vec![];

    for RpcVoteAccountInfo {
        vote_pubkey,
        node_pubkey,
        root_slot,
        ..
    } in &vote_account_info
    {
        let node_pubkey = Pubkey::from_str(&node_pubkey).unwrap();
        let baseline_seed = &vote_pubkey.to_string()[..32];
        let bonus_seed = &format!("A{{{}", vote_pubkey)[..32];
        let vote_pubkey = Pubkey::from_str(&vote_pubkey).unwrap();

        let baseline_stake_address = Pubkey::create_with_seed(
            &config.authorized_staker.pubkey(),
            baseline_seed,
            &solana_stake_program::id(),
        )
        .unwrap();
        let bonus_stake_address = Pubkey::create_with_seed(
            &config.authorized_staker.pubkey(),
            bonus_seed,
            &solana_stake_program::id(),
        )
        .unwrap();

        // Transactions to create the baseline and bonus stake accounts
        if let Ok((balance, _)) = get_stake_account(&rpc_client, &baseline_stake_address) {
            if balance != config.baseline_stake_amount {
                error!(
                    "Unexpected balance in stake account {}: {}, expected {}",
                    baseline_stake_address, balance, config.baseline_stake_amount
                );
                process::exit(1);
            }
        } else {
            source_stake_lamports_required += config.baseline_stake_amount;
            create_stake_transactions.push((
                Transaction::new_unsigned(Message::new_with_payer(
                    &stake_instruction::split_with_seed(
                        &config.source_stake_address,
                        &config.authorized_staker.pubkey(),
                        config.baseline_stake_amount,
                        &baseline_stake_address,
                        &config.authorized_staker.pubkey(),
                        baseline_seed,
                    ),
                    Some(&config.authorized_staker.pubkey()),
                )),
                format!(
                    "Creating baseline stake account for validator {} ({})",
                    node_pubkey, baseline_stake_address
                ),
            ));
        }

        if let Ok((balance, _)) = get_stake_account(&rpc_client, &bonus_stake_address) {
            if balance != config.bonus_stake_amount {
                error!(
                    "Unexpected balance in stake account {}: {}, expected {}",
                    bonus_stake_address, balance, config.bonus_stake_amount
                );
                process::exit(1);
            }
        } else {
            source_stake_lamports_required += config.bonus_stake_amount;
            create_stake_transactions.push((
                Transaction::new_unsigned(Message::new_with_payer(
                    &stake_instruction::split_with_seed(
                        &config.source_stake_address,
                        &config.authorized_staker.pubkey(),
                        config.bonus_stake_amount,
                        &bonus_stake_address,
                        &config.authorized_staker.pubkey(),
                        bonus_seed,
                    ),
                    Some(&config.authorized_staker.pubkey()),
                )),
                format!(
                    "Creating bonus stake account for validator {} ({})",
                    node_pubkey, bonus_stake_address
                ),
            ));
        }

        // Validator is not considered delinquent if its root slot is less than 256 slots behind the current
        // slot.  This is very generous.
        if *root_slot > epoch_info.absolute_slot - 256 {
            if !config.dry_run {
                datapoint_info!(
                    "validator-status",
                    ("cluster", config.metrics_cluster_name, String),
                    ("id", node_pubkey.to_string(), String),
                    ("slot", epoch_info.absolute_slot, i64),
                    ("ok", true, bool)
                );
            }

            // Delegate baseline stake
            delegate_stake_transactions.push((
                Transaction::new_unsigned(Message::new_with_payer(
                    &[stake_instruction::delegate_stake(
                        &baseline_stake_address,
                        &config.authorized_staker.pubkey(),
                        &vote_pubkey,
                    )],
                    Some(&config.authorized_staker.pubkey()),
                )),
                format!(
                    "Validator {} is current, adding {} SOL stake ({})",
                    node_pubkey,
                    lamports_to_sol(config.baseline_stake_amount),
                    baseline_stake_address
                ),
            ));

            if quality_block_producers.contains(&node_pubkey) {
                // Delegate bonus stake
                delegate_stake_transactions.push((
                    Transaction::new_unsigned(
                    Message::new_with_payer(
                        &[stake_instruction::delegate_stake(
                            &bonus_stake_address,
                            &config.authorized_staker.pubkey(),
                            &vote_pubkey,
                        )],
                        Some(&config.authorized_staker.pubkey()),
                    )),
                    format!(
                        "Validator {} produced a block in over {}% of their slots during epoch {}, adding {} SOL stake ({})",
                        node_pubkey,
                        config.quality_block_producer_percentage,
                        last_epoch,
                        lamports_to_sol(config.bonus_stake_amount),
                        bonus_stake_address
                    ),
                ));
            } else {
                // Deactivate bonus stake
                delegate_stake_transactions.push((
                    Transaction::new_unsigned(
                    Message::new_with_payer(
                        &[stake_instruction::deactivate_stake(
                            &bonus_stake_address,
                            &config.authorized_staker.pubkey(),
                        )],
                        Some(&config.authorized_staker.pubkey()),
                    )),
                    format!(
                        "Validator {} produced a block in less than {}% of their slots during epoch {}, removing {} SOL stake ({})",
                        node_pubkey,
                        config.quality_block_producer_percentage,
                        last_epoch,
                        lamports_to_sol(config.bonus_stake_amount),
                        bonus_stake_address
                    ),
                ));
            }
        } else {
            // Destake the validator if it has been delinquent for longer than the grace period
            if *root_slot
                < epoch_info
                    .absolute_slot
                    .saturating_sub(config.delinquent_grace_slot_distance)
            {
                // Deactivate baseline stake
                delegate_stake_transactions.push((
                    Transaction::new_unsigned(Message::new_with_payer(
                        &[stake_instruction::deactivate_stake(
                            &baseline_stake_address,
                            &config.authorized_staker.pubkey(),
                        )],
                        Some(&config.authorized_staker.pubkey()),
                    )),
                    format!(
                        "Validator {} is delinquent, removing {} SOL stake ({}",
                        node_pubkey,
                        lamports_to_sol(config.baseline_stake_amount),
                        baseline_stake_address
                    ),
                ));

                // Deactivate bonus stake
                delegate_stake_transactions.push((
                    Transaction::new_unsigned(Message::new_with_payer(
                        &[stake_instruction::deactivate_stake(
                            &bonus_stake_address,
                            &config.authorized_staker.pubkey(),
                        )],
                        Some(&config.authorized_staker.pubkey()),
                    )),
                    format!(
                        "Validator {} is delinquent, removing {} SOL stake ({}",
                        node_pubkey,
                        lamports_to_sol(config.bonus_stake_amount),
                        bonus_stake_address
                    ),
                ));

                datapoint_info!(
                    "validator-status",
                    ("cluster", config.metrics_cluster_name, String),
                    ("id", node_pubkey.to_string(), String),
                    ("slot", epoch_info.absolute_slot, i64),
                    ("ok", false, bool)
                );
            } else {
                // The validator is still considered current for the purposes of metrics reporting,
                if !config.dry_run {
                    datapoint_info!(
                        "validator-status",
                        ("cluster", config.metrics_cluster_name, String),
                        ("id", node_pubkey.to_string(), String),
                        ("slot", epoch_info.absolute_slot, i64),
                        ("ok", true, bool)
                    );
                }
            }
        }
    }

    if create_stake_transactions.is_empty() {
        info!("All stake accounts exist");
    } else {
        info!(
            "{} SOL is required to create {} stake accounts",
            lamports_to_sol(source_stake_lamports_required),
            create_stake_transactions.len()
        );
        if source_stake_balance < source_stake_lamports_required {
            error!(
                "Source stake account has insufficient balance: {} SOL, but {} SOL is required",
                lamports_to_sol(source_stake_balance),
                lamports_to_sol(source_stake_lamports_required)
            );
            process::exit(1);
        }

        let confirmations = transact(
            &rpc_client,
            config.dry_run,
            create_stake_transactions,
            &config.authorized_staker,
        )?;

        let mut abort = false;
        for (success, memo) in confirmations {
            if success {
                info!("OK - {}", memo);
            } else {
                error!("FAILED - {}", memo);
                abort = true;
            }
        }

        if abort {
            error!("Failed to create one or more stake accounts.  Unable to continue");
            process::exit(1);
        }
    }

    // TODO: filter out `delegate_stake_transactions` transactions that will fail using
    //       https://github.com/solana-labs/solana/issues/8986

    let confirmations = transact(
        &rpc_client,
        config.dry_run,
        delegate_stake_transactions,
        &config.authorized_staker,
    )?;
    for (success, memo) in confirmations {
        info!("{} - {}", if success { "OK" } else { "FAILED" }, memo);
    }

    Ok(())
}
