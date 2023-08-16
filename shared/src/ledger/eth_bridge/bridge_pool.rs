//! Bridge pool SDK functionality.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use borsh::BorshSerialize;
use ethbridge_bridge_contract::Bridge;
use ethers::providers::Middleware;
use namada_core::ledger::eth_bridge::storage::wrapped_erc20s;
use namada_core::types::key::common;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};

use super::{block_on_eth_sync, eth_sync_or_exit, BlockOnEthSync};
use crate::eth_bridge::ethers::abi::AbiDecode;
use crate::eth_bridge::structs::RelayProof;
use crate::ledger::args;
use crate::ledger::queries::{
    Client, GenBridgePoolProofReq, GenBridgePoolProofRsp, RPC,
};
use crate::ledger::rpc::{query_wasm_code_hash, validate_amount};
use crate::ledger::tx::{prepare_tx, Error};
use crate::proto::Tx;
use crate::types::address::Address;
use crate::types::control_flow::time::{Duration, Instant};
use crate::types::control_flow::{
    self, install_shutdown_signal, Halt, TryHalt,
};
use crate::types::eth_abi::Encode;
use crate::types::eth_bridge_pool::{
    GasFee, PendingTransfer, TransferToEthereum, TransferToEthereumKind,
};
use crate::types::keccak::KeccakHash;
use crate::types::token::{Amount, DenominatedAmount};
use crate::types::voting_power::FractionalVotingPower;

/// Craft a transaction that adds a transfer to the Ethereum bridge pool.
pub async fn build_bridge_pool_tx<C: crate::ledger::queries::Client + Sync>(
    client: &C,
    args::EthereumBridgePool {
        tx: tx_args,
        nut,
        asset,
        recipient,
        sender,
        amount,
        fee_amount,
        fee_payer,
        fee_token,
        code_path,
    }: args::EthereumBridgePool,
    gas_payer: common::PublicKey,
) -> Result<Tx, Error> {
    let fee_payer = fee_payer.unwrap_or_else(|| sender.clone());
    let DenominatedAmount { amount, .. } = validate_amount(
        client,
        amount,
        &wrapped_erc20s::token(&asset),
        tx_args.force,
    )
    .await
    .ok_or_else(|| Error::Other("Failed to validate amount".into()))?;
    let DenominatedAmount {
        amount: fee_amount, ..
    } = validate_amount(client, fee_amount, &fee_token, tx_args.force)
        .await
        .ok_or_else(|| {
            Error::Other("Failed to validate Bridge pool fee amount".into())
        })?;
    let transfer = PendingTransfer {
        transfer: TransferToEthereum {
            asset,
            recipient,
            sender,
            amount,
            kind: if nut {
                TransferToEthereumKind::Nut
            } else {
                TransferToEthereumKind::Erc20
            },
        },
        gas_fee: GasFee {
            token: fee_token,
            amount: fee_amount,
            payer: fee_payer,
        },
    };

    let tx_code_hash =
        query_wasm_code_hash(client, code_path.to_str().unwrap())
            .await
            .unwrap();

    let chain_id = tx_args.chain_id.clone().unwrap();
    let mut tx = Tx::new(chain_id, tx_args.expiration);
    tx.add_code_from_hash(tx_code_hash).add_data(transfer);

    prepare_tx::<C>(
        client,
        &tx_args,
        &mut tx,
        gas_payer.clone(),
        #[cfg(not(feature = "mainnet"))]
        false,
    )
    .await;
    Ok(tx)
}

/// A json serializable representation of the Ethereum
/// bridge pool.
#[derive(Serialize, Deserialize)]
struct BridgePoolResponse {
    bridge_pool_contents: HashMap<String, PendingTransfer>,
}

/// Query the contents of the Ethereum bridge pool.
/// Prints out a json payload.
pub async fn query_bridge_pool<C>(client: &C)
where
    C: Client + Sync,
{
    let response: Vec<PendingTransfer> = RPC
        .shell()
        .eth_bridge()
        .read_ethereum_bridge_pool(client)
        .await
        .unwrap();
    let pool_contents: HashMap<String, PendingTransfer> = response
        .into_iter()
        .map(|transfer| (transfer.keccak256().to_string(), transfer))
        .collect();
    if pool_contents.is_empty() {
        println!("Bridge pool is empty.");
        return;
    }
    let contents = BridgePoolResponse {
        bridge_pool_contents: pool_contents,
    };
    println!("{}", serde_json::to_string_pretty(&contents).unwrap());
}

/// Query the contents of the Ethereum bridge pool that
/// is covered by the latest signed root.
/// Prints out a json payload.
pub async fn query_signed_bridge_pool<C>(
    client: &C,
) -> Halt<HashMap<String, PendingTransfer>>
where
    C: Client + Sync,
{
    let response: Vec<PendingTransfer> = RPC
        .shell()
        .eth_bridge()
        .read_signed_ethereum_bridge_pool(client)
        .await
        .unwrap();
    let pool_contents: HashMap<String, PendingTransfer> = response
        .into_iter()
        .map(|transfer| (transfer.keccak256().to_string(), transfer))
        .collect();
    if pool_contents.is_empty() {
        println!("Bridge pool is empty.");
        return control_flow::halt();
    }
    let contents = BridgePoolResponse {
        bridge_pool_contents: pool_contents.clone(),
    };
    println!("{}", serde_json::to_string_pretty(&contents).unwrap());
    control_flow::proceed(pool_contents)
}

/// Iterates over all ethereum events
/// and returns the amount of voting power
/// backing each `TransferToEthereum` event.
///
/// Prints a json payload.
pub async fn query_relay_progress<C>(client: &C)
where
    C: Client + Sync,
{
    let resp = RPC
        .shell()
        .eth_bridge()
        .transfer_to_ethereum_progress(client)
        .await
        .unwrap();
    println!("{}", serde_json::to_string_pretty(&resp).unwrap());
}

/// Internal methdod to construct a proof that a set of transfers are in the
/// bridge pool.
async fn construct_bridge_pool_proof<C>(
    client: &C,
    args: GenBridgePoolProofReq<'_, '_>,
) -> Halt<GenBridgePoolProofRsp>
where
    C: Client + Sync,
{
    let in_progress = RPC
        .shell()
        .eth_bridge()
        .transfer_to_ethereum_progress(client)
        .await
        .unwrap();

    let warnings: Vec<_> = in_progress
        .into_iter()
        .filter_map(|(ref transfer, voting_power)| {
            if voting_power > FractionalVotingPower::ONE_THIRD {
                let hash = transfer.keccak256();
                args.transfers.contains(&hash).then_some(hash)
            } else {
                None
            }
        })
        .collect();

    if !warnings.is_empty() {
        let warning = "Warning".on_yellow();
        let warning = warning.bold();
        let warning = warning.blink();
        println!(
            "{warning}: The following hashes correspond to transfers that \
             have surpassed the security threshold in Namada, therefore have \
             likely been relayed to Ethereum, but do not yet have a quorum of \
             validator signatures behind them in Namada; thus they are still \
             in the Bridge pool:\n{warnings:?}",
        );
        print!("\nDo you wish to proceed? (y/n): ");
        std::io::stdout().flush().unwrap();
        loop {
            let mut buffer = String::new();
            let stdin = std::io::stdin();
            stdin.read_line(&mut buffer).try_halt(|e| {
                println!("Encountered error reading from STDIN: {e:?}");
            })?;
            match buffer.trim() {
                "y" => break,
                "n" => return control_flow::halt(),
                _ => {
                    print!("Expected 'y' or 'n'. Please try again: ");
                    std::io::stdout().flush().unwrap();
                }
            }
        }
    }

    let data = args.try_to_vec().unwrap();
    let response = RPC
        .shell()
        .eth_bridge()
        .generate_bridge_pool_proof(client, Some(data), None, false)
        .await;

    response.map(|response| response.data).try_halt(|e| {
        println!("Encountered error constructing proof:\n{e}");
    })
}

/// A response from construction a bridge pool proof.
#[derive(Serialize)]
struct BridgePoolProofResponse {
    hashes: Vec<KeccakHash>,
    relayer_address: Address,
    total_fees: HashMap<Address, Amount>,
    abi_encoded_proof: Vec<u8>,
}

/// Construct a merkle proof of a batch of transfers in
/// the bridge pool and return it to the user (as opposed
/// to relaying it to ethereum).
pub async fn construct_proof<C>(
    client: &C,
    args: args::BridgePoolProof,
) -> Halt<()>
where
    C: Client + Sync,
{
    let GenBridgePoolProofRsp {
        abi_encoded_proof: bp_proof_bytes,
        appendices,
    } = construct_bridge_pool_proof(
        client,
        GenBridgePoolProofReq {
            transfers: args.transfers.as_slice().into(),
            relayer: Cow::Borrowed(&args.relayer),
            with_appendix: true,
        },
    )
    .await?;
    let resp = BridgePoolProofResponse {
        hashes: args.transfers,
        relayer_address: args.relayer,
        total_fees: appendices
            .map(|appendices| {
                appendices.into_iter().fold(
                    HashMap::new(),
                    |mut total_fees, app| {
                        let GasFee { token, amount, .. } =
                            app.gas_fee.into_owned();
                        let fees = total_fees
                            .entry(token)
                            .or_insert_with(Amount::zero);
                        fees.receive(&amount);
                        total_fees
                    },
                )
            })
            .unwrap_or_default(),
        abi_encoded_proof: bp_proof_bytes,
    };
    println!("{}", serde_json::to_string(&resp).unwrap());
    control_flow::proceed(())
}

/// Relay a validator set update, signed off for a given epoch.
pub async fn relay_bridge_pool_proof<C, E>(
    eth_client: Arc<E>,
    nam_client: &C,
    args: args::RelayBridgePoolProof,
) -> Halt<()>
where
    C: Client + Sync,
    E: Middleware,
    E::Error: std::fmt::Debug + std::fmt::Display,
{
    let _signal_receiver = args.safe_mode.then(install_shutdown_signal);

    if args.sync {
        block_on_eth_sync(
            &*eth_client,
            BlockOnEthSync {
                deadline: Instant::now() + Duration::from_secs(60),
                delta_sleep: Duration::from_secs(1),
            },
        )
        .await?;
    } else {
        eth_sync_or_exit(&*eth_client).await?;
    }

    let GenBridgePoolProofRsp {
        abi_encoded_proof: bp_proof,
        ..
    } = construct_bridge_pool_proof(
        nam_client,
        GenBridgePoolProofReq {
            transfers: Cow::Owned(args.transfers),
            relayer: Cow::Owned(args.relayer),
            with_appendix: false,
        },
    )
    .await?;
    let bridge = match RPC
        .shell()
        .eth_bridge()
        .read_bridge_contract(nam_client)
        .await
    {
        Ok(address) => Bridge::new(address.address, eth_client),
        Err(err_msg) => {
            let error = "Error".on_red();
            let error = error.bold();
            let error = error.blink();
            println!(
                "{error}: Failed to retrieve the Ethereum Bridge smart \
                 contract address from storage with \
                 reason:\n{err_msg}\n\nPerhaps the Ethereum bridge is not \
                 active.",
            );
            return control_flow::halt();
        }
    };

    let bp_proof: RelayProof =
        AbiDecode::decode(&bp_proof).try_halt(|error| {
            println!("Unable to decode the generated proof: {:?}", error);
        })?;

    // NOTE: this operation costs no gas on Ethereum
    let contract_nonce =
        bridge.transfer_to_erc_20_nonce().call().await.unwrap();

    match bp_proof.batch_nonce.cmp(&contract_nonce) {
        Ordering::Equal => {}
        Ordering::Less => {
            let error = "Error".on_red();
            let error = error.bold();
            let error = error.blink();
            println!(
                "{error}: The Bridge pool nonce in the smart contract is \
                 {contract_nonce}, while the nonce in Namada is still {}. A \
                 relay of the former one has already happened, but a proof \
                 has yet to be crafted in Namada.",
                bp_proof.batch_nonce
            );
            return control_flow::halt();
        }
        Ordering::Greater => {
            let error = "Error".on_red();
            let error = error.bold();
            let error = error.blink();
            println!(
                "{error}: The Bridge pool nonce in the smart contract is \
                 {contract_nonce}, while the nonce in Namada is still {}. \
                 Somehow, Namada's nonce is ahead of the contract's nonce!",
                bp_proof.batch_nonce
            );
            return control_flow::halt();
        }
    }

    let mut relay_op = bridge.transfer_to_erc(bp_proof);
    if let Some(gas) = args.gas {
        relay_op.tx.set_gas(gas);
    }
    if let Some(gas_price) = args.gas_price {
        relay_op.tx.set_gas_price(gas_price);
    }
    if let Some(eth_addr) = args.eth_addr {
        relay_op.tx.set_from(eth_addr.into());
    }

    let pending_tx = relay_op.send().await.unwrap();
    let transf_result = pending_tx
        .confirmations(args.confirmations as usize)
        .await
        .unwrap();

    println!("{transf_result:?}");
    control_flow::proceed(())
}

mod recommendations {
    use borsh::BorshDeserialize;
    use namada_core::types::uint::{self, Uint, I256};

    use super::*;
    use crate::eth_bridge::storage::bridge_pool::get_signed_root_key;
    use crate::eth_bridge::storage::proof::BridgePoolRootProof;
    use crate::types::storage::BlockHeight;
    use crate::types::vote_extensions::validator_set_update::{
        EthAddrBook, VotingPowersMap, VotingPowersMapExt,
    };

    const fn unsigned_transfer_fee() -> Uint {
        Uint::from_u64(37_500_u64)
    }

    const fn transfer_fee() -> I256 {
        I256(unsigned_transfer_fee())
    }

    const fn signature_fee() -> Uint {
        Uint::from_u64(24_500)
    }

    const fn valset_fee() -> Uint {
        Uint::from_u64(2000)
    }

    /// The different states while trying to solve
    /// for a recommended batch of transfers.
    struct AlgorithState {
        /// We are scanning transfers that increase
        /// net profits to the relayer. However, we
        /// are not in the feasible region.
        profitable: bool,
        /// We are scanning solutions that satisfy the
        /// requirements of the input.
        feasible_region: bool,
    }

    /// The algorithm exhibits two different remmondation strategies
    /// depending on whether the user is will to accept a positive cost
    /// for relaying.
    #[derive(PartialEq)]
    enum AlgorithmMode {
        /// Only keep profitable transactions
        Greedy,
        /// Allow transactions which are not profitable
        Generous,
    }

    /// Transfer to Ethereum that is eligible to be recommended
    /// for a relay operation, generating a profit.
    ///
    /// This means that the underlying Ethereum event has not
    /// been "seen" yet, and that the user provided appropriate
    /// conversion rates to gwei for the gas fee token in
    /// the transfer.
    struct EligibleRecommendation {
        /// Pending transfer to Ethereum.
        pending_transfer: PendingTransfer,
        /// Hash of the [`PendingTransfer`].
        transfer_hash: String,
        /// Cost of relaying the transfer, in gwei.
        cost: I256,
    }

    /// Recommend the most economical batch of transfers to relay based
    /// on a conversion rate estimates from NAM to ETH and gas usage
    /// heuristics.
    pub async fn recommend_batch<C>(
        client: &C,
        args: args::RecommendBatch,
    ) -> Halt<()>
    where
        C: Client + Sync,
    {
        // get transfers that can already been relayed but are awaiting a quorum
        // of backing votes.
        let in_progress = RPC
            .shell()
            .eth_bridge()
            .transfer_to_ethereum_progress(client)
            .await
            .unwrap()
            .into_keys()
            .collect::<Vec<_>>();

        // get the signed bridge pool root so we can analyze the signatures
        // the estimate the gas cost of verifying them.
        let (bp_root, height) =
            <(BridgePoolRootProof, BlockHeight)>::try_from_slice(
                &RPC.shell()
                    .storage_value(
                        client,
                        None,
                        Some(0.into()),
                        false,
                        &get_signed_root_key(),
                    )
                    .await
                    .unwrap()
                    .data,
            )
            .unwrap();

        // Get the voting powers of each of validator who signed
        // the above root.
        let voting_powers = RPC
            .shell()
            .eth_bridge()
            .voting_powers_at_height(client, &height)
            .await
            .unwrap();
        let valset_size = Uint::from_u64(voting_powers.len() as u64);

        // This is the gas cost for hashing the validator set and
        // checking a quorum of signatures (in gwei).
        let validator_gas = signature_fee()
            * signature_checks(voting_powers, &bp_root.signatures)
            + valset_fee() * valset_size;

        // we don't recommend transfers that have already been relayed
        let mut contents: Vec<EligibleRecommendation> =
            query_signed_bridge_pool(client)
                .await?
                .into_iter()
                .filter_map(|(pending_hash, pending)| {
                    if !in_progress.contains(&pending) {
                        let conversion_rate =
                            if let Some(entry) = args
                                .conversion_table
                                .get(&pending.gas_fee.token)
                            {
                                let rate = entry.conversion_rate;
                                if rate <= 0.0f64 {
                                    eprintln!(
                                        "Ignoring token with an invalid conversion rate: {}",
                                        pending.gas_fee.token,
                                    );
                                    return None;
                                }
                                rate
                            } else {
                                return None;
                            };
                        // This is the amount of gwei a single gas token is worth
                        let gwei_per_gas_token = Uint::from_u64(
                            (10u64.pow(9) as f64 / conversion_rate).floor() as u64,
                        );
                        Some((
                            pending_hash,
                            I256::try_from(pending.gas_fee.amount * gwei_per_gas_token)
                                .map(|cost| transfer_fee() - cost)
                                .try_halt(|err| {
                                    tracing::debug!(%err, "Failed to convert value to I256");
                                }),
                            pending,
                        ))
                    } else {
                        None
                    }
                })
                .try_fold(Vec::new(), |mut accum, (hash, cost, transf)| {
                    accum.push(EligibleRecommendation {
                        cost: cost?,
                        transfer_hash: hash,
                        pending_transfer: transf,
                    });
                    control_flow::proceed(accum)
                })?;

        // sort transfers in decreasing amounts of profitability
        contents.sort_by_key(|EligibleRecommendation { cost, .. }| *cost);

        let max_gas =
            args.max_gas.map(Uint::from_u64).unwrap_or(uint::MAX_VALUE);
        let max_cost = args.gas.map(I256::from).unwrap_or_default();
        generate(
            contents,
            &args.conversion_table,
            validator_gas,
            max_gas,
            max_cost,
        )?;

        control_flow::proceed(())
    }

    /// Given an ordered list of signatures, figure out the size of the first
    /// subset constituting a 2 / 3 majority.
    ///
    /// The function is generic to make unit testing easier (otherwise a dev
    /// dependency needs to be added).
    fn signature_checks<T>(
        voting_powers: VotingPowersMap,
        sigs: &HashMap<EthAddrBook, T>,
    ) -> Uint {
        let voting_powers = voting_powers.get_sorted();
        let total_power = voting_powers.iter().map(|(_, &y)| y).sum::<Amount>();

        // Find the total number of signature checks Ethereum will make
        let mut power = FractionalVotingPower::NULL;
        Uint::from_u64(
            voting_powers
                .iter()
                .filter_map(|(a, &p)| sigs.get(a).map(|_| p))
                .take_while(|p| {
                    if power <= FractionalVotingPower::TWO_THIRDS {
                        power += FractionalVotingPower::new(
                            (*p).into(),
                            total_power.into(),
                        )
                        .unwrap();
                        true
                    } else {
                        false
                    }
                })
                .count() as u64,
        )
    }

    /// Generates the actual recommendation from restrictions given by the
    /// input parameters.
    fn generate(
        contents: Vec<EligibleRecommendation>,
        conversion_table: &HashMap<Address, args::BpConversionTableEntry>,
        validator_gas: Uint,
        max_gas: Uint,
        max_cost: I256,
    ) -> Halt<Option<Vec<String>>> {
        let mut state = AlgorithState {
            profitable: true,
            feasible_region: false,
        };

        let mode = if max_cost <= I256::zero() {
            AlgorithmMode::Greedy
        } else {
            AlgorithmMode::Generous
        };

        let mut total_gas = validator_gas;
        let mut total_cost = I256::try_from(validator_gas).try_halt(|err| {
            tracing::debug!(%err, "Failed to convert value to I256");
        })?;
        let mut total_fees = HashMap::new();
        let mut recommendation = vec![];
        for EligibleRecommendation {
            cost,
            transfer_hash: hash,
            pending_transfer: transfer,
        } in contents.into_iter()
        {
            let next_total_gas = total_gas + unsigned_transfer_fee();
            let next_total_cost = total_cost + cost;
            if cost.is_negative() {
                if next_total_gas <= max_gas && next_total_cost <= max_cost {
                    state.feasible_region = true;
                } else if state.feasible_region {
                    // once we leave the feasible region, we will never re-enter
                    // it.
                    break;
                }
                recommendation.push(hash);
            } else if mode == AlgorithmMode::Generous {
                state.profitable = false;
                let is_feasible =
                    next_total_gas <= max_gas && next_total_cost <= max_cost;
                // once we leave the feasible region, we will never re-enter it.
                if state.feasible_region && !is_feasible {
                    break;
                } else {
                    recommendation.push(hash);
                }
            } else {
                break;
            }
            total_cost = next_total_cost;
            total_gas = next_total_gas;
            update_total_fees(&mut total_fees, transfer, conversion_table);
        }

        control_flow::proceed(
            if state.feasible_region && !recommendation.is_empty() {
                println!("Recommended batch: {:#?}", recommendation);
                println!(
                    "Estimated Ethereum transaction gas (in gwei): {}",
                    total_gas
                );
                println!("Estimated net profit (in gwei): {}", -total_cost);
                println!("Total fees: {total_fees:#?}");
                Some(recommendation)
            } else {
                println!(
                    "Unable to find a recommendation satisfying the input \
                     parameters."
                );
                None
            },
        )
    }

    fn update_total_fees(
        total_fees: &mut HashMap<String, Uint>,
        transfer: PendingTransfer,
        conversion_table: &HashMap<Address, args::BpConversionTableEntry>,
    ) {
        let GasFee { token, amount, .. } = transfer.gas_fee;
        let fees = total_fees
            .entry(
                conversion_table
                    .get(&token)
                    .map(|entry| entry.alias.clone())
                    .unwrap_or_else(|| token.to_string()),
            )
            .or_insert(uint::ZERO);
        *fees += Uint::from(amount);
    }

    #[cfg(test)]
    mod test_recommendations {
        use namada_core::types::address::Address;
        use namada_core::types::ethereum_events::EthAddress;

        use super::*;
        use crate::types::control_flow::ProceedOrElse;

        /// An established user address for testing & development
        pub fn bertha_address() -> Address {
            Address::decode(
                "atest1v4ehgw36xvcyyvejgvenxs34g3zygv3jxqunjd6rxyeyys3sxy6rwvfkx4qnj33hg9qnvse4lsfctw",
            )
            .expect("The token address decoding shouldn't fail")
        }

        /// Generate a pending transfer with the specified gas
        /// fee.
        pub fn transfer(gas_amount: u64) -> PendingTransfer {
            PendingTransfer {
                transfer: TransferToEthereum {
                    kind: TransferToEthereumKind::Erc20,
                    asset: EthAddress([1; 20]),
                    recipient: EthAddress([2; 20]),
                    sender: bertha_address(),
                    amount: Default::default(),
                },
                gas_fee: GasFee {
                    token: namada_core::types::address::nam(),
                    amount: gas_amount.into(),
                    payer: bertha_address(),
                },
            }
        }

        /// Convert transfers into a format that the `generate` function
        /// understands.
        fn process_transfers(
            transfers: Vec<PendingTransfer>,
        ) -> Vec<EligibleRecommendation> {
            transfers
                .into_iter()
                .map(|t| EligibleRecommendation {
                    cost: transfer_fee() - t.gas_fee.amount.change(),
                    transfer_hash: t.keccak256().to_string(),
                    pending_transfer: t,
                })
                .collect()
        }

        fn address_book(i: u8) -> EthAddrBook {
            EthAddrBook {
                hot_key_addr: EthAddress([i; 20]),
                cold_key_addr: EthAddress([i; 20]),
            }
        }

        #[test]
        fn test_signature_count() {
            let voting_powers = VotingPowersMap::from([
                (address_book(1), Amount::from(5)),
                (address_book(2), Amount::from(1)),
                (address_book(3), Amount::from(1)),
            ]);
            let signatures = HashMap::from([
                (address_book(1), 0),
                (address_book(2), 0),
                (address_book(3), 0),
            ]);
            let checks = signature_checks(voting_powers, &signatures);
            assert_eq!(checks, uint::ONE)
        }

        #[test]
        fn test_signature_count_with_skips() {
            let voting_powers = VotingPowersMap::from([
                (address_book(1), Amount::from(5)),
                (address_book(2), Amount::from(5)),
                (address_book(3), Amount::from(1)),
                (address_book(4), Amount::from(1)),
            ]);
            let signatures = HashMap::from([
                (address_book(1), 0),
                (address_book(3), 0),
                (address_book(4), 0),
            ]);
            let checks = signature_checks(voting_powers, &signatures);
            assert_eq!(checks, Uint::from_u64(3))
        }

        #[test]
        fn test_only_profitable() {
            let profitable = vec![transfer(100_000); 17];
            let hash = profitable[0].keccak256().to_string();
            let expected = vec![hash; 17];
            let recommendation = generate(
                process_transfers(profitable),
                &Default::default(),
                Uint::from_u64(800_000),
                uint::MAX_VALUE,
                I256::zero(),
            )
            .proceed()
            .expect("Test failed");
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_non_profitable_removed() {
            let mut transfers = vec![transfer(100_000); 17];
            let hash = transfers[0].keccak256().to_string();
            transfers.push(transfer(0));
            let expected: Vec<_> = vec![hash; 17];
            let recommendation = generate(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(800_000),
                uint::MAX_VALUE,
                I256::zero(),
            )
            .proceed()
            .expect("Test failed");
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_max_gas() {
            let transfers = vec![transfer(75_000); 4];
            let hash = transfers[0].keccak256().to_string();
            let expected = vec![hash; 2];
            let recommendation = generate(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(50_000),
                Uint::from_u64(150_000),
                I256(uint::MAX_SIGNED_VALUE),
            )
            .proceed()
            .expect("Test failed");
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_net_loss() {
            let mut transfers = vec![transfer(75_000); 4];
            transfers.extend([transfer(17_500), transfer(17_500)]);
            let expected: Vec<_> = transfers
                .iter()
                .map(|t| t.keccak256().to_string())
                .take(5)
                .collect();
            let recommendation = generate(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(150_000),
                uint::MAX_VALUE,
                I256::from(20_000),
            )
            .proceed()
            .expect("Test failed");
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_net_loss_max_gas() {
            let mut transfers = vec![transfer(75_000); 4];
            let hash = transfers[0].keccak256().to_string();
            let expected = vec![hash; 4];
            transfers.extend([transfer(17_500), transfer(17_500)]);
            let recommendation = generate(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(150_000),
                Uint::from_u64(330_000),
                I256::from(20_000),
            )
            .proceed()
            .expect("Test failed");
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_wholly_infeasible() {
            let transfers = vec![transfer(75_000); 4];
            let recommendation = generate(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(300_000),
                uint::MAX_VALUE,
                I256::from(20_000),
            )
            .proceed();
            assert!(recommendation.is_none())
        }
    }
}

pub use recommendations::recommend_batch;
