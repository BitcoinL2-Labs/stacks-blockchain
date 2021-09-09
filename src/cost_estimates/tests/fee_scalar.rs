use std::{env, path::PathBuf};
use time::Instant;

use rand::seq::SliceRandom;
use rand::Rng;

use cost_estimates::metrics::CostMetric;
use cost_estimates::{EstimatorError, FeeEstimator};
use vm::costs::ExecutionCost;

use chainstate::burn::ConsensusHash;
use chainstate::stacks::db::{StacksEpochReceipt, StacksHeaderInfo};
use chainstate::stacks::events::StacksTransactionReceipt;
use types::chainstate::{BlockHeaderHash, BurnchainHeaderHash, StacksBlockHeader, StacksWorkScore};
use types::proof::TrieHash;
use util::hash::{to_hex, Hash160, Sha512Trunc256Sum};
use util::vrf::VRFProof;

use crate::chainstate::stacks::{
    CoinbasePayload, StacksTransaction, TokenTransferMemo, TransactionAuth,
    TransactionContractCall, TransactionPayload, TransactionSpendingCondition, TransactionVersion,
};
use crate::cost_estimates::fee_scalar::ScalarFeeRateEstimator;
use crate::cost_estimates::FeeRateEstimate;
use crate::types::chainstate::StacksAddress;
use crate::vm::types::{PrincipalData, StandardPrincipalData};
use crate::vm::Value;

fn instantiate_test_db<CM: CostMetric>(m: CM) -> ScalarFeeRateEstimator<CM> {
    let mut path = env::temp_dir();
    let random_bytes = rand::thread_rng().gen::<[u8; 32]>();
    path.push(&format!("fee_db_{}.sqlite", &to_hex(&random_bytes)[0..8]));

    ScalarFeeRateEstimator::open(&path, m).expect("Test failure: could not open fee rate DB")
}

/// This struct implements a simple metric used for unit testing the
/// the fee rate estimator. It always returns a cost of 1, making the
/// fee rate of a transaction always equal to the paid fee.
struct TestCostMetric;

impl CostMetric for TestCostMetric {
    fn from_cost_and_len(&self, _cost: &ExecutionCost, _tx_len: u64) -> u64 {
        1
    }

    fn from_len(&self, _tx_len: u64) -> u64 {
        1
    }
}

#[cfg(test)]
fn test_empty_fee_estimator() {
    let metric = TestCostMetric;
    let estimator = instantiate_test_db(metric);
    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect_err("Empty rate estimator should error."),
        EstimatorError::NoEstimateAvailable
    );
}

fn make_block_receipt(tx_receipts: Vec<StacksTransactionReceipt>) -> StacksEpochReceipt {
    StacksEpochReceipt {
        header: StacksHeaderInfo {
            anchored_header: StacksBlockHeader {
                version: 1,
                total_work: StacksWorkScore { burn: 1, work: 1 },
                proof: VRFProof::empty(),
                parent_block: BlockHeaderHash([0; 32]),
                parent_microblock: BlockHeaderHash([0; 32]),
                parent_microblock_sequence: 0,
                tx_merkle_root: Sha512Trunc256Sum([0; 32]),
                state_index_root: TrieHash([0; 32]),
                microblock_pubkey_hash: Hash160([0; 20]),
            },
            microblock_tail: None,
            block_height: 1,
            index_root: TrieHash([0; 32]),
            consensus_hash: ConsensusHash([2; 20]),
            burn_header_hash: BurnchainHeaderHash([1; 32]),
            burn_header_height: 2,
            burn_header_timestamp: 2,
            anchored_block_size: 1,
        },
        tx_receipts,
        matured_rewards: vec![],
        matured_rewards_info: None,
        parent_microblocks_cost: ExecutionCost::zero(),
        anchored_block_cost: ExecutionCost::zero(),
        parent_burn_block_hash: BurnchainHeaderHash([0; 32]),
        parent_burn_block_height: 1,
        parent_burn_block_timestamp: 1,
    }
}

fn make_dummy_coinbase_tx() -> StacksTransaction {
    StacksTransaction::new(
        TransactionVersion::Mainnet,
        TransactionAuth::Standard(TransactionSpendingCondition::new_initial_sighash()),
        TransactionPayload::Coinbase(CoinbasePayload([0; 32])),
    )
}

fn make_dummy_transfer_tx(fee: u64) -> StacksTransactionReceipt {
    let mut tx = StacksTransaction::new(
        TransactionVersion::Mainnet,
        TransactionAuth::Standard(TransactionSpendingCondition::new_initial_sighash()),
        TransactionPayload::TokenTransfer(
            PrincipalData::Standard(StandardPrincipalData(0, [0; 20])),
            1,
            TokenTransferMemo([0; 34]),
        ),
    );
    tx.set_tx_fee(fee);

    StacksTransactionReceipt::from_stx_transfer(
        tx,
        vec![],
        Value::okay(Value::Bool(true)).unwrap(),
        ExecutionCost::zero(),
    )
}

fn make_dummy_cc_tx(fee: u64) -> StacksTransactionReceipt {
    let mut tx = StacksTransaction::new(
        TransactionVersion::Mainnet,
        TransactionAuth::Standard(TransactionSpendingCondition::new_initial_sighash()),
        TransactionPayload::ContractCall(TransactionContractCall {
            address: StacksAddress::new(0, Hash160([0; 20])),
            contract_name: "cc-dummy".into(),
            function_name: "func-name".into(),
            function_args: vec![],
        }),
    );
    tx.set_tx_fee(fee);
    StacksTransactionReceipt::from_contract_call(
        tx,
        vec![],
        Value::okay(Value::Bool(true)).unwrap(),
        0,
        ExecutionCost::zero(),
    )
}

#[test]
fn test_fee_estimator() {
    let metric = TestCostMetric;
    let mut estimator = instantiate_test_db(metric);

    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect_err("Empty rate estimator should error."),
        EstimatorError::NoEstimateAvailable,
        "Empty rate estimator should return no estimate available"
    );

    let empty_block_receipt = make_block_receipt(vec![]);
    estimator
        .notify_block(&empty_block_receipt)
        .expect("Should be able to process an empty block");

    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect_err("Empty rate estimator should error."),
        EstimatorError::NoEstimateAvailable,
        "Empty block should not update the estimator"
    );

    let coinbase_only_receipt = make_block_receipt(vec![StacksTransactionReceipt::from_coinbase(
        make_dummy_coinbase_tx(),
    )]);

    estimator
        .notify_block(&coinbase_only_receipt)
        .expect("Should be able to process an empty block");

    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect_err("Empty rate estimator should error."),
        EstimatorError::NoEstimateAvailable,
        "Coinbase-only block should not update the estimator"
    );

    let single_tx_receipt = make_block_receipt(vec![
        StacksTransactionReceipt::from_coinbase(make_dummy_coinbase_tx()),
        make_dummy_cc_tx(1),
    ]);

    estimator
        .notify_block(&single_tx_receipt)
        .expect("Should be able to process block receipt");

    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 1,
            medium: 1,
            slow: 1
        }
    );

    let double_tx_receipt = make_block_receipt(vec![
        StacksTransactionReceipt::from_coinbase(make_dummy_coinbase_tx()),
        make_dummy_cc_tx(1),
        make_dummy_transfer_tx(10),
    ]);

    estimator
        .notify_block(&double_tx_receipt)
        .expect("Should be able to process block receipt");

    // estimate should increase for "fast" and "medium":
    // 10 * 1/2 + 1 * 1/2 = 5
    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 5,
            medium: 5,
            slow: 1
        }
    );

    // estimate should increase for "fast" and "medium":
    // new value: 10 * 1/2 + 5 * 1/2 = 7
    estimator
        .notify_block(&double_tx_receipt)
        .expect("Should be able to process block receipt");
    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 7,
            medium: 7,
            slow: 1
        }
    );

    // estimate should increase for "fast" and "medium":
    // new value: 10 * 1/2 + 7 * 1/2 = 8
    estimator
        .notify_block(&double_tx_receipt)
        .expect("Should be able to process block receipt");
    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 8,
            medium: 8,
            slow: 1
        }
    );

    // estimate should increase for "fast" and "medium":
    // new value: 10 * 1/2 + 8 * 1/2 = 9
    estimator
        .notify_block(&double_tx_receipt)
        .expect("Should be able to process block receipt");
    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 9,
            medium: 9,
            slow: 1
        }
    );

    // estimate should increase for "fast" and "medium":
    // new value: 10 * 1/2 + 9 * 1/2 = 9
    // note: we get a little "stuck" by the integer weighting here: 9/2 = 4.5, and 10/2 = 5, so we get stuck at 9,
    //       even though if we had more accuracy, we'd move to 10 on the estimate. This isn't too damaging in practice:
    //       fee rates are expressed in microstx, which should have much more resolution than this.
    estimator
        .notify_block(&double_tx_receipt)
        .expect("Should be able to process block receipt");
    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 9,
            medium: 9,
            slow: 1
        }
    );

    // make a large block receipt, and expect:
    //  measured fast = 950, medium = 500, slow = 50
    //  new fast: 950/2 + 9/2 = 475 + 4 = 479
    //  new medium: 500/2 + 9/2 = 250 + 4 = 254
    //  new slow: 50/2 + 1/2 = 25 + 0 = 25

    let mut receipts: Vec<_> = (0..100).map(|i| make_dummy_cc_tx(i * 10)).collect();
    let mut rng = rand::thread_rng();
    receipts.shuffle(&mut rng);

    estimator
        .notify_block(&make_block_receipt(receipts))
        .expect("Should be able to process block receipt");

    assert_eq!(
        estimator
            .get_rate_estimates()
            .expect("Should be able to create estimate now"),
        FeeRateEstimate {
            fast: 479,
            medium: 254,
            slow: 25
        }
    );
}