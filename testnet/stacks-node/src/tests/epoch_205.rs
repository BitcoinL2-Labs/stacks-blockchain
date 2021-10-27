use std::env;
use std::thread;

use stacks::burnchains::Burnchain;
use stacks::burnchains::Txid;
use stacks::chainstate::burn::operations::BlockstackOperationType;
use stacks::chainstate::stacks::StacksPrivateKey;
use stacks::chainstate::stacks::StacksTransaction;
use stacks::chainstate::stacks::TransactionPayload;
use stacks::codec::StacksMessageCodec;
use stacks::types::chainstate::BlockHeaderHash;
use stacks::types::chainstate::BurnchainHeaderHash;
use stacks::types::chainstate::StacksAddress;
use stacks::util::hash::hex_bytes;
use stacks::vm::types::PrincipalData;

use crate::config::EventKeyType;
use crate::config::EventObserverConfig;
use crate::config::InitialBalance;
use crate::neon;
use crate::tests::bitcoin_regtest::BitcoinCoreController;
use crate::tests::make_contract_call;
use crate::tests::make_contract_publish;
use crate::tests::neon_integrations::*;
use crate::tests::to_addr;
use crate::BitcoinRegtestController;
use crate::BurnchainController;
use crate::Keychain;
use stacks::core;

use stacks::chainstate::burn::operations::leader_block_commit::BURN_BLOCK_MINED_AT_MODULUS;
use stacks::chainstate::burn::operations::LeaderBlockCommitOp;
use stacks::types::chainstate::VRFSeed;

#[test]
#[ignore]
// Test dynamic db method costs by invoking a db fetch operation
//  with the same data schema, but with varying db size. Check
//  that when the epoch boundary crosses, the cost difference between
//  the operations changes.
fn test_dynamic_db_method_costs() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let spender_sk = StacksPrivateKey::new();
    let spender_addr = PrincipalData::from(to_addr(&spender_sk));
    let spender_addr_c32 = StacksAddress::from(to_addr(&spender_sk));
    let contract_name = "test-contract";

    let epoch_205_transition_height = 210;

    let contract_content = "
      (define-data-var db1 (list 500 int) (list 1 2 3 4 5))
      (define-data-var db2 (list 500 int) (list 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20))
      (define-public (db-get1)
        (begin (var-get db1)
               (ok 1)))
      (define-public (db-get2)
        (begin (var-get db2)
               (ok 1)))
    ";

    let (mut conf, _miner_account) = neon_integration_test_conf();
    let mut epochs = core::STACKS_EPOCHS_REGTEST.to_vec();
    epochs[1].end_height = epoch_205_transition_height;
    epochs[2].start_height = epoch_205_transition_height;

    conf.burnchain.epochs = Some(epochs);

    conf.initial_balances.push(InitialBalance {
        address: spender_addr.clone(),
        amount: 200_000_000,
    });

    let contract_publish_tx =
        make_contract_publish(&spender_sk, 0, 210_000, contract_name, contract_content);

    let make_db_get1_call = |nonce| {
        make_contract_call(
            &spender_sk,
            nonce,
            200_000,
            &spender_addr_c32,
            contract_name,
            "db-get1",
            &[],
        )
    };

    let make_db_get2_call = |nonce| {
        make_contract_call(
            &spender_sk,
            nonce,
            200_000,
            &spender_addr_c32,
            contract_name,
            "db-get2",
            &[],
        )
    };

    test_observer::spawn();
    conf.events_observers.push(EventObserverConfig {
        endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller
        .start_bitcoind()
        .map_err(|_e| ())
        .expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    btc_regtest_controller.bootstrap_chain(201);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf.clone());
    let blocks_processed = run_loop.get_blocks_processed_arc();

    let channel = run_loop.get_coordinator_channel().unwrap();

    thread::spawn(move || run_loop.start(None, 0));

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // second block will be the first mined Stacks block
    //  include the testing contract publish tx
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tip_info = get_chain_info(&conf);
    let current_burn_height = tip_info.burn_block_height as u32;
    assert_eq!(current_burn_height, 204);
    submit_tx(&http_origin, &contract_publish_tx);

    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tip_info = get_chain_info(&conf);
    let current_burn_height = tip_info.burn_block_height as u32;
    assert_eq!(current_burn_height, 205);

    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tip_info = get_chain_info(&conf);
    let current_burn_height = tip_info.burn_block_height as u32;
    assert_eq!(current_burn_height, 206);

    // broadcast 10 contract calls and produce 10 more blocks
    //  there's an off-by-one behavior in `next_block_and_wait`, where the miner
    //  has already assembled the next block when it is called, so the tx broadcasted
    //  when current_burn_height = `n` will be included in a block elected at burn height
    //  `n + 2`
    for i in 0..10 {
        submit_tx(&http_origin, &make_db_get1_call(1 + (2 * i)));
        submit_tx(&http_origin, &make_db_get2_call(2 + (2 * i)));
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    let tip_info = get_chain_info(&conf);
    let current_burn_height = tip_info.burn_block_height as u32;
    assert_eq!(current_burn_height, 216);

    let blocks = test_observer::get_blocks();

    let mut tested_heights = vec![];

    for block in blocks {
        let burn_height = block.get("burn_block_height").unwrap().as_i64().unwrap();
        let transactions = block.get("transactions").unwrap().as_array().unwrap();

        let mut db1_cost = None;
        let mut db2_cost = None;
        for tx in transactions.iter() {
            let raw_tx = tx.get("raw_tx").unwrap().as_str().unwrap();
            if raw_tx == "0x00" {
                continue;
            }
            let tx_bytes = hex_bytes(&raw_tx[2..]).unwrap();
            let parsed = StacksTransaction::consensus_deserialize(&mut &tx_bytes[..]).unwrap();

            if let TransactionPayload::ContractCall(ref cc) = parsed.payload {
                assert_eq!(
                    cc.contract_name.as_str(),
                    contract_name,
                    "All contract calls should be to the test contract"
                );

                let function_name = cc.function_name.as_str();
                assert!(function_name == "db-get1" || function_name == "db-get2");

                let runtime_cost = tx
                    .get("execution_cost")
                    .unwrap()
                    .get("runtime")
                    .unwrap()
                    .as_i64()
                    .unwrap();
                eprintln!(
                    "Burn height = {}, runtime_cost = {}, function_name = {}",
                    burn_height, runtime_cost, function_name
                );

                if function_name == "db-get1" {
                    db1_cost = Some(runtime_cost);
                } else if function_name == "db-get2" {
                    db2_cost = Some(runtime_cost);
                }
            }
        }

        if let Some(db1_cost) = db1_cost {
            tested_heights.push(burn_height as u64);
            let db2_cost = db2_cost.expect("`db-get1` was called in block without `db-get2`");
            if burn_height <= epoch_205_transition_height as i64 {
                assert_eq!(
                    db1_cost, db2_cost,
                    "In Epoch 2.0, the cost of `db-get1` and `db-get2` should be equal"
                );
            } else {
                assert!(
                    db1_cost < db2_cost,
                    "In Epoch 2.05, the cost of `db-get1` should be less than `db-get2`"
                );
            };
        }
    }

    // make sure that the test covered the blocks before, at, and after the epoch transition.
    assert!(tested_heights.contains(&(epoch_205_transition_height - 1)));
    assert!(tested_heights.contains(&epoch_205_transition_height));
    assert!(tested_heights.contains(&(epoch_205_transition_height + 1)));

    test_observer::clear();
    channel.stop_chains_coordinator();
}

#[test]
#[ignore]
fn transition_empty_blocks() {
    // very simple test to verify that the miner will keep making valid (empty) blocks after the
    // transition.  Really tests that the block-commits are well-formed before and after the epoch
    // transition.
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let epoch_2_05 = 210;

    let (mut conf, miner_account) = neon_integration_test_conf();

    let mut epochs = core::STACKS_EPOCHS_REGTEST.to_vec();
    epochs[1].end_height = epoch_2_05;
    epochs[2].start_height = epoch_2_05;

    conf.burnchain.epochs = Some(epochs);

    let keychain = Keychain::default(conf.node.seed.clone());

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller
        .start_bitcoind()
        .map_err(|_e| ())
        .expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    btc_regtest_controller.bootstrap_chain(epoch_2_05 - 5);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf.clone());
    let blocks_processed = run_loop.get_blocks_processed_arc();

    let channel = run_loop.get_coordinator_channel().unwrap();

    thread::spawn(move || run_loop.start(None, 0));

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tip_info = get_chain_info(&conf);
    let key_block_ptr = tip_info.burn_block_height as u32;
    let key_vtxindex = 1; // nothing else here but the coinbase

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let mut bitcoin_controller = BitcoinRegtestController::new_dummy(conf.clone());
    let burnchain = Burnchain::regtest(&conf.get_burn_db_path());

    // these should all succeed across the epoch boundary
    for _i in 0..5 {
        // also, make *huge* block-commits with invalid marker bytes once we reach the new
        // epoch, and verify that it fails.
        let tip_info = get_chain_info(&conf);
        if tip_info.burn_block_height + 1 >= epoch_2_05 {
            let burn_fee_cap = 100000000; // 1 BTC
            let sunset_burn =
                burnchain.expected_sunset_burn(tip_info.burn_block_height + 1, burn_fee_cap);
            let rest_commit = burn_fee_cap - sunset_burn;

            let commit_outs = if tip_info.burn_block_height + 1 < burnchain.pox_constants.sunset_end
                && !burnchain.is_in_prepare_phase(tip_info.burn_block_height + 1)
            {
                vec![
                    StacksAddress::burn_address(conf.is_mainnet()),
                    StacksAddress::burn_address(conf.is_mainnet()),
                ]
            } else {
                vec![StacksAddress::burn_address(conf.is_mainnet())]
            };

            // let's commit
            let burn_parent_modulus =
                (tip_info.burn_block_height % BURN_BLOCK_MINED_AT_MODULUS) as u8;
            let op = BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
                sunset_burn,
                block_header_hash: BlockHeaderHash([0xff; 32]),
                burn_fee: rest_commit,
                input: (Txid([0; 32]), 0),
                apparent_sender: keychain.get_burnchain_signer(),
                key_block_ptr,
                key_vtxindex,
                memo: vec![0], // bad epoch marker
                new_seed: VRFSeed([0x11; 32]),
                parent_block_ptr: 0,
                parent_vtxindex: 0,
                // to be filled in
                vtxindex: 0,
                txid: Txid([0u8; 32]),
                block_height: 0,
                burn_header_hash: BurnchainHeaderHash::zero(),
                burn_parent_modulus,
                commit_outs,
            });
            let mut op_signer = keychain.generate_op_signer();
            let res = bitcoin_controller.submit_operation(op, &mut op_signer, 1);
            assert!(res, "Failed to submit block-commit");
        }

        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    let account = get_account(&http_origin, &miner_account);
    assert_eq!(account.nonce, 6);

    channel.stop_chains_coordinator();
}