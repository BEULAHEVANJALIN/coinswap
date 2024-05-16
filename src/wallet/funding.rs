//! Various mechanisms of creating the swap funding transactions.
//!
//! This module contains routines for creating funding transactions within a wallet. It leverages
//! Bitcoin Core's RPC methods for wallet interactions, including `walletcreatefundedpsbt`

use std::{collections::HashMap, iter};

use bitcoin::{
    absolute::LockTime,
    // address::NetworkUnchecked,
    Address,
    Amount,
    OutPoint,
    ScriptBuf,
    Sequence,
    Transaction,
    TxIn,
    TxOut,
    Txid,
    Witness,
};

use bitcoind::bitcoincore_rpc::{
    json::{
        CreateRawTransactionInput,
        //  WalletCreateFundedPsbtOptions
    },
    RpcApi,
};

// use serde_json::Value;

use bitcoin::secp256k1::rand::{rngs::OsRng, RngCore};

// use crate::utill::convert_json_rpc_bitcoin_to_satoshis;

use super::Wallet;

use super::error::WalletError;

pub struct CreateFundingTxesResult {
    pub funding_txes: Vec<Transaction>,
    pub payment_output_positions: Vec<u32>,
    pub total_miner_fee: u64,
}

impl Wallet {
    // Attempts to create the funding transactions.
    /// Returns Ok(None) if there was no error but the wallet was unable to create funding txes
    pub fn create_funding_txes(
        &self,
        coinswap_amount: u64,
        destinations: &[Address],
        fee_rate: u64,
    ) -> Result<CreateFundingTxesResult, WalletError> {
        let ret = self.create_funding_txes_random_amounts(coinswap_amount, destinations, fee_rate);
        if ret.is_ok() {
            log::info!(target: "wallet", "created funding txes with random amounts");
            return ret;
        }

        let ret = self.create_funding_txes_utxo_max_sends(coinswap_amount, destinations, fee_rate);
        if ret.is_ok() {
            log::info!(target: "wallet", "created funding txes with fully-spending utxos");
            return ret;
        }

        let ret =
            self.create_funding_txes_use_biggest_utxos(coinswap_amount, destinations, fee_rate);
        if ret.is_ok() {
            log::info!(target: "wallet", "created funding txes with using the biggest utxos");
            return ret;
        }

        log::info!(target: "wallet", "failed to create funding txes with any method");
        ret
    }

    fn generate_amount_fractions_without_correction(
        count: usize,
        total_amount: u64,
        lower_limit: u64,
    ) -> Result<Vec<f32>, WalletError> {
        for _ in 0..100000 {
            let mut knives = (1..count)
                .map(|_| (OsRng.next_u32() as f32) / (u32::MAX as f32))
                .collect::<Vec<f32>>();
            knives.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

            let mut fractions = Vec::<f32>::new();
            let mut last: f32 = 1.0;
            for k in knives {
                fractions.push(last - k);
                last = k;
            }
            fractions.push(last);

            if fractions
                .iter()
                .all(|f| *f * (total_amount as f32) > (lower_limit as f32))
            {
                return Ok(fractions);
            }
        }
        Err(WalletError::Protocol(
            "Unable to generate amount fractions, probably amount too small".to_string(),
        ))
    }

    pub fn generate_amount_fractions(
        count: usize,
        total_amount: u64,
    ) -> Result<Vec<u64>, WalletError> {
        let mut output_values = Wallet::generate_amount_fractions_without_correction(
            count,
            total_amount,
            5000, //use 5000 satoshi as the lower limit for now
                  //there should always be enough to pay miner fees
        )?
        .iter()
        .map(|f| (*f * (total_amount as f32)) as u64)
        .collect::<Vec<u64>>();

        //rounding errors mean usually 1 or 2 satoshis are lost, add them back

        //this calculation works like this:
        //o = [a, b, c, ...]             | list of output values
        //t = coinswap amount            | total desired value
        //a' <-- a + (t - (a+b+c+...))   | assign new first output value
        //a' <-- a + (t -a-b-c-...)      | rearrange
        //a' <-- t - b - c -...          |
        *output_values.first_mut().unwrap() =
            total_amount - output_values.iter().skip(1).sum::<u64>();
        assert_eq!(output_values.iter().sum::<u64>(), total_amount);

        Ok(output_values)
    }

    /// This function creates funding txes by
    /// Randomly generating some satoshi amounts and send them into
    /// walletcreatefundedpsbt to create txes that create change
    fn create_funding_txes_random_amounts(
        &self,
        coinswap_amount: u64,
        destinations: &[Address],
        _fee_rate: u64,
    ) -> Result<CreateFundingTxesResult, WalletError> {
        // let _ = self.construct_pbst(coinswap_amount, destinations, fee_rate);
        let change_addresses = self.get_next_internal_addresses(destinations.len() as u32)?;

        let output_values = Wallet::generate_amount_fractions(destinations.len(), coinswap_amount)?;

        self.lock_unspendable_utxos()?;

        let mut funding_txes = Vec::<Transaction>::new();
        let mut payment_output_positions = Vec::<u32>::new();
        let mut total_miner_fee = 0;
        for ((address, &output_value), change_address) in destinations
            .iter()
            .zip(output_values.iter())
            .zip(change_addresses.iter())
        {
            let mut outputs = HashMap::<String, Amount>::new();
            outputs.insert(address.to_string(), Amount::from_sat(output_value));

            // let change_addrs_unchecked: Address<NetworkUnchecked> = change_address
            //     .to_string()
            //     .parse()
            //     .unwrap();

            // let wcfp_result = self.rpc.wallet_create_funded_psbt(
            //     &[],
            //     &outputs,
            //     None,
            //     Some(WalletCreateFundedPsbtOptions {
            //         include_watching: Some(true),
            //         change_address: Some(change_addrs_unchecked),
            //         fee_rate: Some(Amount::from_sat(fee_rate)),
            //         ..Default::default()
            //     }),
            //     None
            // )?;
            // total_miner_fee += wcfp_result.fee.to_sat();

            // let funding_tx = self.from_walletcreatefundedpsbt_to_tx(&wcfp_result.psbt)?;
            let fee = Amount::from_sat(50000);
            let remaining = Amount::from_sat(coinswap_amount);
            let selected_utxo = self.coin_select(remaining)?;
            let total_input_amount = selected_utxo.iter().fold(Amount::ZERO, |acc, (unspet, _)| {
                acc.checked_add(unspet.amount)
                    .expect("Amount sum overflowed")
            });
            let change_amount = total_input_amount.checked_sub(remaining + fee);
            let mut tx_outs = vec![TxOut {
                value: output_value,
                script_pubkey: address.script_pubkey(),
            }];

            if let Some(_x) = change_amount {
                tx_outs.push(TxOut {
                    value: change_amount.expect("expected").to_sat(),
                    script_pubkey: change_address.script_pubkey(),
                });
            }
            let tx_inputs = selected_utxo
                .iter()
                .map(|(unspent, _)| TxIn {
                    previous_output: OutPoint::new(unspent.txid, unspent.vout),
                    sequence: Sequence(0),
                    witness: Witness::new(),
                    script_sig: ScriptBuf::new(),
                })
                .collect::<Vec<_>>();
            let mut funding_tx = Transaction {
                input: tx_inputs,
                output: tx_outs,
                lock_time: LockTime::ZERO,
                version: 2, // anti-fee-snipping
            };
            let mut input_info = selected_utxo
                .iter()
                .map(|(_, spend_info)| spend_info.clone());
            self.sign_transaction(&mut funding_tx, &mut input_info)?;
            // let txid = self.rpc.send_raw_transaction(&funding_tx)?;
            self.rpc.lock_unspent(
                &funding_tx
                    .input
                    .iter()
                    .map(|vin| vin.previous_output)
                    .collect::<Vec<OutPoint>>(),
            )?;

            let payment_pos = 1;

            funding_txes.push(funding_tx);
            payment_output_positions.push(payment_pos);
            total_miner_fee += 50000;
        }

        Ok(CreateFundingTxesResult {
            funding_txes,
            payment_output_positions,
            total_miner_fee,
        })
    }

    fn create_mostly_sweep_txes_with_one_tx_having_change(
        &self,
        coinswap_amount: u64,
        destinations: &[Address],
        _fee_rate: u64,
        change_address: &Address,
        utxos: &mut dyn Iterator<Item = (Txid, u32, u64)>, //utxos item is (txid, vout, value)
                                                           //utxos should be sorted by size, largest first
    ) -> Result<CreateFundingTxesResult, WalletError> {
        let mut funding_txes = Vec::<Transaction>::new();
        let mut payment_output_positions = Vec::<u32>::new();
        let mut total_miner_fee = 0;

        let mut leftover_coinswap_amount = coinswap_amount;
        let mut destinations_iter = destinations.iter();
        let first_tx_input = utxos.next().unwrap();

        for _ in 0..destinations.len() - 2 {
            let (txid, vout, value) = utxos.next().unwrap();

            let mut outputs = HashMap::<&Address, u64>::new();
            outputs.insert(destinations_iter.next().unwrap(), value);
            let tx_inputs = vec![TxIn {
                previous_output: OutPoint::new(txid, vout),
                sequence: Sequence(0),
                witness: Witness::new(),
                script_sig: ScriptBuf::new(),
            }];
            let mut input_info = iter::once(self.get_utxo((txid, vout))?.unwrap());

            let mut tx_outs = Vec::new();
            for (address, value) in outputs {
                tx_outs.push(TxOut {
                    value,
                    script_pubkey: address.script_pubkey(),
                });
            }
            let mut funding_tx = Transaction {
                input: tx_inputs,
                output: tx_outs,
                lock_time: LockTime::ZERO,
                version: 2,
            };
            self.sign_transaction(&mut funding_tx, &mut input_info)?;
            // if input_info.is_none() {
            //     return WalletError::Protocol("Cannot find the utxo".to_string());
            // }

            // let wcfp_result = self.rpc.wallet_create_funded_psbt(
            //     &[
            //         CreateRawTransactionInput {
            //             txid,
            //             vout,
            //             sequence: None,
            //         },
            //     ],
            //     &outputs,
            //     None,
            //     Some(WalletCreateFundedPsbtOptions {
            //         add_inputs: Some(false),
            //         subtract_fee_from_outputs: vec![0],
            //         fee_rate: Some(Amount::from_sat(fee_rate)),
            //         ..Default::default()
            //     }),
            //     None
            // )?;
            // let funding_tx = self.from_walletcreatefundedpsbt_to_tx(&wcfp_result.psbt)?;
            leftover_coinswap_amount -= funding_tx.output[0].value;

            total_miner_fee += 50000;

            funding_txes.push(funding_tx);
            payment_output_positions.push(0);
        }
        let mut tx_inputs = Vec::new();
        let mut input_info = Vec::new();
        let (_leftover_inputs, leftover_inputs_values): (Vec<_>, Vec<_>) = utxos
            .map(|(txid, vout, value)| {
                tx_inputs.push(TxIn {
                    previous_output: OutPoint::new(txid, vout),
                    sequence: Sequence(0),
                    witness: Witness::new(),
                    script_sig: ScriptBuf::new(),
                });
                input_info.push(self.get_utxo((txid, vout)).unwrap().unwrap());
                (
                    CreateRawTransactionInput {
                        txid,
                        vout,
                        sequence: None,
                    },
                    value,
                )
            })
            .unzip();
        let mut outputs = HashMap::<&Address, u64>::new();
        outputs.insert(
            destinations_iter.next().unwrap(),
            leftover_inputs_values.iter().sum::<u64>(),
        );
        let mut tx_outs = Vec::new();
        for (address, value) in outputs {
            tx_outs.push(TxOut {
                value,
                script_pubkey: address.script_pubkey(),
            });
        }
        let mut funding_tx = Transaction {
            input: tx_inputs,
            output: tx_outs,
            lock_time: LockTime::ZERO,
            version: 2,
        };
        let mut info = input_info.iter().cloned();
        self.sign_transaction(&mut funding_tx, &mut info)?;
        // let wcfp_result = self.rpc.wallet_create_funded_psbt(
        //     &leftover_inputs,
        //     &outputs,
        //     None,
        //     Some(WalletCreateFundedPsbtOptions {
        //         add_inputs: Some(false),
        //         subtract_fee_from_outputs: vec![0],
        //         fee_rate: Some(Amount::from_sat(fee_rate)),
        //         ..Default::default()
        //     }),
        //     None
        // )?;
        // let funding_tx = self.from_walletcreatefundedpsbt_to_tx(&wcfp_result.psbt)?;
        leftover_coinswap_amount -= funding_tx.output[0].value;

        total_miner_fee += 50000;

        funding_txes.push(funding_tx);
        payment_output_positions.push(0);

        let (first_txid, first_vout, first_value) = first_tx_input;
        let mut outputs = HashMap::<&Address, u64>::new();
        outputs.insert(destinations_iter.next().unwrap(), leftover_coinswap_amount);
        // let change_addrs_unchecked: Address<NetworkUnchecked> = change_address
        //     .to_string()
        //     .parse()
        //     .unwrap();

        tx_inputs = Vec::new();
        tx_outs = Vec::new();
        let mut change_amount = first_value;
        tx_inputs.push(TxIn {
            previous_output: OutPoint::new(first_txid, first_vout),
            sequence: Sequence(0),
            witness: Witness::new(),
            script_sig: ScriptBuf::new(),
        });
        for (address, value) in outputs {
            change_amount -= value;
            tx_outs.push(TxOut {
                value,
                script_pubkey: address.script_pubkey(),
            });
        }
        tx_outs.push(TxOut {
            value: change_amount,
            script_pubkey: change_address.script_pubkey(),
        });
        let mut funding_tx = Transaction {
            input: tx_inputs,
            output: tx_outs,
            lock_time: LockTime::ZERO,
            version: 2,
        };
        let mut info = iter::once(self.get_utxo((first_txid, first_vout))?.unwrap());
        self.sign_transaction(&mut funding_tx, &mut info)?;
        // let wcfp_result = self.rpc.wallet_create_funded_psbt(
        //     &[
        //         CreateRawTransactionInput {
        //             txid: first_txid,
        //             vout: first_vout,
        //             sequence: None,
        //         },
        //     ],
        //     &outputs,
        //     None,
        //     Some(WalletCreateFundedPsbtOptions {
        //         add_inputs: Some(false),
        //         change_address: Some(change_addrs_unchecked),
        //         fee_rate: Some(Amount::from_sat(fee_rate)),
        //         ..Default::default()
        //     }),
        //     None
        // )?;
        // let funding_tx = self.from_walletcreatefundedpsbt_to_tx(&wcfp_result.psbt)?;

        total_miner_fee += 50000;

        funding_txes.push(funding_tx);
        // payment_output_positions.push(if wcfp_result.change_position == 0 { 1 } else { 0 });
        payment_output_positions.push(1);

        Ok(CreateFundingTxesResult {
            funding_txes,
            payment_output_positions,
            total_miner_fee,
        })
    }

    fn create_funding_txes_utxo_max_sends(
        &self,
        coinswap_amount: u64,
        destinations: &[Address],
        fee_rate: u64,
    ) -> Result<CreateFundingTxesResult, WalletError> {
        //this function creates funding txes by
        //using walletcreatefundedpsbt for the total amount, and if
        //the number if inputs UTXOs is >number_of_txes then split those inputs into groups
        //across multiple transactions

        let mut outputs = HashMap::<&Address, u64>::new();
        outputs.insert(&destinations[0], coinswap_amount);
        let change_address = self.get_next_internal_addresses(1)?[0].clone();

        // let change_addrs_uncheked: Address<NetworkUnchecked> = change_address
        //     .to_string()
        //     .parse()
        //     .unwrap();

        self.lock_unspendable_utxos()?;

        let fee = Amount::from_sat(50000);

        let remaining = Amount::from_sat(coinswap_amount);

        let selected_utxo = self.coin_select(remaining)?;

        let total_input_amount = selected_utxo.iter().fold(Amount::ZERO, |acc, (unspet, _)| {
            acc.checked_add(unspet.amount)
                .expect("Amount sum overflowed")
        });
        let change_amount = total_input_amount.checked_sub(remaining + fee);

        let mut tx_outs = vec![TxOut {
            value: coinswap_amount,
            script_pubkey: destinations[0].script_pubkey(),
        }];

        if let Some(_x) = change_amount {
            tx_outs.push(TxOut {
                value: change_amount.expect("expected").to_sat(),
                script_pubkey: change_address.script_pubkey(),
            });
        }

        let tx_inputs = selected_utxo
            .iter()
            .map(|(unspent, _)| TxIn {
                previous_output: OutPoint::new(unspent.txid, unspent.vout),
                sequence: Sequence(0),
                witness: Witness::new(),
                script_sig: ScriptBuf::new(),
            })
            .collect::<Vec<_>>();

        let mut funding_tx = Transaction {
            input: tx_inputs,
            output: tx_outs,
            lock_time: LockTime::ZERO,
            version: 2, // anti-fee-snipping
        };

        let mut input_info = selected_utxo
            .iter()
            .map(|(_, spend_info)| spend_info.clone());
        self.sign_transaction(&mut funding_tx, &mut input_info)?;

        // let wcfp_result = self.rpc.wallet_create_funded_psbt(
        //     &[],
        //     &outputs,
        //     None,
        //     Some(WalletCreateFundedPsbtOptions {
        //         include_watching: Some(true),
        //         change_address: Some(change_addrs_uncheked),
        //         fee_rate: Some(Amount::from_sat(fee_rate)),
        //         ..Default::default()
        //     }),
        //     None
        // )?;
        // //TODO rust-bitcoin handles psbt, use those functions instead
        // let decoded_psbt = self.rpc.call::<Value>(
        //     "decodepsbt",
        //     &[Value::String(wcfp_result.psbt)]
        // )?;
        // let total_tx_inputs_len = decoded_psbt["inputs"].as_array().unwrap().len();
        let total_tx_inputs_len = selected_utxo.len();
        if total_tx_inputs_len < destinations.len() {
            return Err(WalletError::Protocol(
                "not enough UTXOs found, cant use this method".to_string(),
            ));
        }

        // let mut total_tx_inputs = decoded_psbt["tx"]["vin"]
        //     .as_array()
        //     .unwrap()
        //     .iter()
        //     .zip(decoded_psbt["inputs"].as_array().unwrap().iter())
        //     .collect::<Vec<(&Value, &Value)>>();

        // total_tx_inputs.sort_by(|(_, a), (_, b)| {
        //     b["witness_utxo"]["amount"]
        //         .as_f64()
        //         .unwrap()
        //         .partial_cmp(&a["witness_utxo"]["amount"].as_f64().unwrap())
        //         .unwrap_or(std::cmp::Ordering::Equal)
        // });

        self.create_mostly_sweep_txes_with_one_tx_having_change(
            coinswap_amount,
            destinations,
            fee_rate,
            &change_address,
            &mut selected_utxo
                .iter()
                .map(|(l, _)| (l.txid, l.vout, l.amount.to_sat())),
        )
    }

    fn create_funding_txes_use_biggest_utxos(
        &self,
        coinswap_amount: u64,
        destinations: &[Address],
        fee_rate: u64,
    ) -> Result<CreateFundingTxesResult, WalletError> {
        //this function will pick the top most valuable UTXOs and use them
        //to create funding transactions

        let all_utxos = self.get_all_utxo()?;

        let mut seed_coin_utxo = self.list_descriptor_utxo_spend_info(Some(&all_utxos))?;
        let mut swap_coin_utxo = self.list_swap_coin_utxo_spend_info(Some(&all_utxos))?;
        seed_coin_utxo.append(&mut swap_coin_utxo);

        let mut list_unspent_result = seed_coin_utxo;
        if list_unspent_result.len() < destinations.len() {
            return Err(WalletError::Protocol(
                "Not enough UTXOs to create this many funding txes".to_string(),
            ));
        }
        list_unspent_result.sort_by(|(a, _), (b, _)| {
            b.amount
                .to_sat()
                .partial_cmp(&a.amount.to_sat())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut list_unspent_count: Option<usize> = None;
        for ii in destinations.len()..list_unspent_result.len() + 1 {
            let sum = list_unspent_result[..ii]
                .iter()
                .map(|(l, _)| l.amount.to_sat())
                .sum::<u64>();
            if sum > coinswap_amount {
                list_unspent_count = Some(ii);
                break;
            }
        }
        if list_unspent_count.is_none() {
            return Err(WalletError::Protocol(
                "Not enough UTXOs/value to create funding txes".to_string(),
            ));
        }

        let inputs = &list_unspent_result[..list_unspent_count.unwrap()];

        if inputs[1..]
            .iter()
            .map(|(l, _)| l.amount.to_sat())
            .any(|utxo_value| utxo_value > coinswap_amount)
        {
            // TODO: Handle this case
            Err(WalletError::Protocol(
                "Some stupid error that will never occur".to_string(),
            ))
        } else {
            //at most one utxo bigger than the coinswap amount

            let change_address = &self.get_next_internal_addresses(1)?[0];
            self.create_mostly_sweep_txes_with_one_tx_having_change(
                coinswap_amount,
                destinations,
                fee_rate,
                change_address,
                &mut inputs.iter().map(|(list_unspent_entry, _spend_info)| {
                    (
                        list_unspent_entry.txid,
                        list_unspent_entry.vout,
                        list_unspent_entry.amount.to_sat(),
                    )
                }),
            )
        }
    }
}
