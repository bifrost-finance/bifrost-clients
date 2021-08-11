// #![cfg(feature = "integration")]

use bitcoin::{stream_blocks, BitcoinCoreApi};
use frame_support::assert_ok;
use futures::{
    channel::mpsc,
    future::{join, join3, join5},
    Future, FutureExt, SinkExt, TryStreamExt,
};
use runtime::{
    integration::*,
    pallets::{
        issue::*, redeem::*, refund::*, replace::*, security::UpdateActiveBlockEvent, tokens::*, vault_registry::*,
    },
    BtcAddress, BtcRelayPallet, ExchangeRateOraclePallet, FixedPointNumber, FixedU128, InterBtcParachain,
    InterBtcRedeemRequest, InterBtcRuntime, IssuePallet, RedeemPallet, ReplacePallet, UtilFuncs, VaultRegistryPallet,
};
use sp_core::{H160, H256};
use sp_keyring::AccountKeyring;
use std::{sync::Arc, time::Duration};
use vault::{self, Event as CancellationEvent, IssueRequests};

const TIMEOUT: Duration = Duration::from_secs(60);

async fn test_with<F, R>(execute: impl FnOnce(SubxtClient) -> F) -> R
where
    F: Future<Output = R>,
{
    service::init_subscriber();
    let (client, _tmp_dir) = default_provider_client(AccountKeyring::Alice).await;

    let parachain_rpc = setup_provider(client.clone(), AccountKeyring::Bob).await;

    set_exchange_rate(&parachain_rpc, FixedU128::saturating_from_rational(1u128, 100u128)).await;
    assert_ok!(parachain_rpc.set_bitcoin_fees(FixedU128::from(0)).await);

    execute(client).await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_redeem_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&vault_provider, issue_amount).await;
        assert_ok!(
            vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(&user_provider, &btc_rpc, vault_provider.get_account_id(), issue_amount).await;

        test_service(
            vault::service::listen_for_redeem_requests(vault_provider.clone(), btc_rpc, 0, Duration::from_secs(0)),
            async {
                let address = BtcAddress::P2PKH(H160::from_slice(&[2; 20]));
                let vault_id = vault_provider.clone().get_account_id().clone();
                let redeem_id = user_provider.request_redeem(10000, address, &vault_id).await.unwrap();
                assert_redeem_event(TIMEOUT, user_provider, redeem_id).await;
                // TODO: check bitcoin payment amount
            },
        )
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_replace_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let old_vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let new_vault_provider = setup_provider(client.clone(), AccountKeyring::Eve).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&old_vault_provider, issue_amount).await;
        assert_ok!(
            old_vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );
        assert_ok!(
            new_vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(
            &user_provider,
            &btc_rpc,
            old_vault_provider.get_account_id(),
            issue_amount,
        )
        .await;

        let old_vault_id = old_vault_provider.get_account_id().clone();
        let new_vault_id = new_vault_provider.get_account_id().clone();

        let (replace_event_tx, _) = mpsc::channel::<CancellationEvent>(16);
        test_service(
            join(
                vault::service::listen_for_replace_requests(
                    new_vault_provider.clone(),
                    btc_rpc.clone(),
                    replace_event_tx.clone(),
                    true,
                ),
                vault::service::listen_for_accept_replace(
                    old_vault_provider.clone(),
                    btc_rpc.clone(),
                    0,
                    Duration::from_secs(0),
                ),
            ),
            async {
                old_vault_provider.request_replace(issue_amount, 1000000).await.unwrap();

                assert_event::<AcceptReplaceEvent<InterBtcRuntime>, _>(TIMEOUT, old_vault_provider.clone(), |e| {
                    assert_eq!(e.old_vault_id, old_vault_id);
                    assert_eq!(e.new_vault_id, new_vault_id);
                    true
                })
                .await;
                assert_event::<ExecuteReplaceEvent<InterBtcRuntime>, _>(TIMEOUT, old_vault_provider.clone(), |e| {
                    assert_eq!(e.old_vault_id, old_vault_id);
                    assert_eq!(e.new_vault_id, new_vault_id);
                    true
                })
                .await;
            },
        )
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_maintain_collateral_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&vault_provider, issue_amount).await;
        assert_ok!(
            vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(&user_provider, &btc_rpc, vault_provider.get_account_id(), issue_amount).await;

        test_service(
            vault::service::maintain_collateralization_rate(vault_provider.clone(), Some(1000000000)),
            async {
                // dot per btc increases by 10%
                set_exchange_rate(
                    &relayer_provider,
                    FixedU128::saturating_from_rational(110u128, 10000u128),
                )
                .await;
                set_exchange_rate(
                    &relayer_provider,
                    FixedU128::saturating_from_rational(110u128, 10000u128),
                )
                .await;

                assert_event::<DepositCollateralEvent<InterBtcRuntime>, _>(TIMEOUT, vault_provider.clone(), |e| {
                    assert_eq!(e.new_collateral, vault_collateral / 10);
                    true
                })
                .await;
            },
        )
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_withdraw_replace_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let old_vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let new_vault_provider = setup_provider(client.clone(), AccountKeyring::Eve).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&old_vault_provider, issue_amount).await;
        assert_ok!(
            old_vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );
        assert_ok!(
            new_vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(
            &user_provider,
            &btc_rpc,
            old_vault_provider.get_account_id(),
            issue_amount,
        )
        .await;

        join(
            old_vault_provider
                .request_replace(issue_amount, 1000000)
                .map(Result::unwrap),
            assert_event::<RequestReplaceEvent<InterBtcRuntime>, _>(TIMEOUT, old_vault_provider.clone(), |_| true),
        )
        .await;

        let old_vault_id = old_vault_provider.get_account_id().clone();

        join(
            old_vault_provider.withdraw_replace(issue_amount).map(Result::unwrap),
            assert_event::<WithdrawReplaceEvent<InterBtcRuntime>, _>(TIMEOUT, old_vault_provider.clone(), |e| {
                assert_eq!(e.old_vault_id, old_vault_id);
                true
            }),
        )
        .await;

        let address = BtcAddress::P2PKH(H160::from_slice(&[2; 20]));
        assert!(new_vault_provider
            .accept_replace(&old_vault_id, 1u32.into(), vault_collateral, address)
            .await
            .is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancellation_succeeds() {
    // tests cancellation of issue, redeem and replace.
    // issue and replace cancellation is tested through the vault's cancellation service.
    // cancel_redeem is called manually
    test_with(|client| async move {
        let root_provider = setup_provider(client.clone(), AccountKeyring::Alice).await;
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let old_vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let new_vault_provider = setup_provider(client.clone(), AccountKeyring::Eve).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&old_vault_provider, issue_amount * 10).await;
        assert_ok!(
            old_vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );
        assert_ok!(
            new_vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(
            &user_provider,
            &btc_rpc,
            old_vault_provider.get_account_id(),
            issue_amount,
        )
        .await;

        // set low timeout periods
        assert_ok!(root_provider.set_issue_period(1).await);
        assert_ok!(root_provider.set_replace_period(1).await);
        assert_ok!(root_provider.set_redeem_period(1).await);

        let (issue_cancellation_event_tx, issue_cancellation_rx) = mpsc::channel::<CancellationEvent>(16);
        let (replace_cancellation_event_tx, replace_cancellation_rx) = mpsc::channel::<CancellationEvent>(16);

        let block_listener = new_vault_provider.clone();
        let issue_set = Arc::new(IssueRequests::new());

        let issue_request_listener = vault::service::listen_for_issue_requests(
            btc_rpc.clone(),
            new_vault_provider.clone(),
            issue_cancellation_event_tx.clone(),
            issue_set.clone(),
        );

        let mut issue_cancellation_scheduler = vault::service::CancellationScheduler::new(
            new_vault_provider.clone(),
            new_vault_provider.get_current_chain_height().await.unwrap(),
            0,
            new_vault_provider.get_account_id().clone(),
        );
        let mut replace_cancellation_scheduler = vault::service::CancellationScheduler::new(
            new_vault_provider.clone(),
            new_vault_provider.get_current_chain_height().await.unwrap(),
            0,
            new_vault_provider.get_account_id().clone(),
        );
        let issue_canceller =
            issue_cancellation_scheduler.handle_cancellation::<vault::service::IssueCanceller>(issue_cancellation_rx);
        let replace_canceller = replace_cancellation_scheduler
            .handle_cancellation::<vault::service::ReplaceCanceller>(replace_cancellation_rx);

        let parachain_block_listener = async {
            let issue_block_tx = &issue_cancellation_event_tx.clone();
            let replace_block_tx = &replace_cancellation_event_tx.clone();

            block_listener
                .clone()
                .on_event::<UpdateActiveBlockEvent<InterBtcRuntime>, _, _, _>(
                    |event| async move {
                        assert_ok!(
                            issue_block_tx
                                .clone()
                                .send(CancellationEvent::ParachainBlock(event.height))
                                .await
                        );
                        assert_ok!(
                            replace_block_tx
                                .clone()
                                .send(CancellationEvent::ParachainBlock(event.height))
                                .await
                        );
                    },
                    |_err| (),
                )
                .await
                .unwrap();
        };

        let initial_btc_height = btc_rpc.get_block_count().await.unwrap() as u32;
        let bitcoin_block_listener = async {
            let issue_block_tx = &issue_cancellation_event_tx.clone();
            let replace_block_tx = &replace_cancellation_event_tx.clone();

            stream_blocks(btc_rpc.clone(), initial_btc_height, 1)
                .await
                .try_for_each(|_| async {
                    let height = btc_rpc.get_block_count().await? as u32;
                    let _ = issue_block_tx
                        .clone()
                        .send(CancellationEvent::BitcoinBlock(height))
                        .await;
                    let _ = replace_block_tx
                        .clone()
                        .send(CancellationEvent::BitcoinBlock(height))
                        .await;
                    Ok(())
                })
                .await
                .unwrap();
        };
        let old_vault_id = old_vault_provider.get_account_id();

        test_service(
            join5(
                issue_canceller.map(Result::unwrap),
                replace_canceller.map(Result::unwrap),
                issue_request_listener.map(Result::unwrap),
                parachain_block_listener,
                bitcoin_block_listener,
            ),
            async {
                let address = BtcAddress::P2PKH(H160::from_slice(&[2; 20]));

                // setup the to-be-cancelled redeem
                let redeem_id = user_provider
                    .request_redeem(20000, address, old_vault_provider.get_account_id())
                    .await
                    .unwrap();

                join3(
                    async {
                        // setup the to-be-cancelled replace
                        assert_ok!(old_vault_provider.request_replace(issue_amount / 2, 1000000).await);
                        assert_ok!(
                            new_vault_provider
                                .accept_replace(&old_vault_id, 10000000u32.into(), 0u32.into(), address)
                                .await
                        );
                        assert_ok!(
                            replace_cancellation_event_tx
                                .clone()
                                .send(CancellationEvent::Opened)
                                .await
                        );

                        // setup the to-be-cancelled issue
                        assert_ok!(
                            user_provider
                                .request_issue(issue_amount, new_vault_provider.get_account_id(), 10000)
                                .await
                        );

                        for _ in 0u32..2 {
                            assert_ok!(
                                btc_rpc
                                    .send_to_address(BtcAddress::P2PKH(H160::from_slice(&[0; 20])), 100_000, None, 1)
                                    .await
                            );
                        }
                    },
                    assert_event::<CancelIssueEvent<InterBtcRuntime>, _>(
                        Duration::from_secs(120),
                        user_provider.clone(),
                        |_| true,
                    ),
                    assert_event::<CancelReplaceEvent<InterBtcRuntime>, _>(
                        Duration::from_secs(120),
                        user_provider.clone(),
                        |_| true,
                    ),
                )
                .await;

                // now make sure we can cancel the redeem
                assert_ok!(user_provider.cancel_redeem(redeem_id, true).await);
            },
        )
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_refund_succeeds() {
    test_with(|client| async move {
        let sudo_provider = setup_provider(client.clone(), AccountKeyring::Alice).await;
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let refund_service = vault::service::listen_for_refund_requests(vault_provider.clone(), btc_rpc.clone(), 0);

        assert_ok!(sudo_provider.set_parachain_confirmations(0).await);

        let issue_amount = 100000;
        let vault_collateral = 2 * get_required_vault_collateral_for_issue(&vault_provider, issue_amount).await;
        assert_ok!(
            vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        let vault_id = vault_provider.get_account_id().clone();
        let fut_user = async {
            let over_payment = 100500;

            let issue = user_provider
                .request_issue(issue_amount, vault_provider.get_account_id(), 10000)
                .await
                .unwrap();

            let metadata = btc_rpc
                .send_to_address(
                    issue.vault_btc_address,
                    (issue.amount_btc + issue.fee) as u64 + over_payment,
                    None,
                    0,
                )
                .await
                .unwrap();

            let (_, refund_request, refund_execution) = join3(
                user_provider.execute_issue(issue.issue_id, &metadata.proof, &metadata.raw_tx),
                // overpayment on execute_issue should emit this event
                assert_event::<RequestRefundEvent<InterBtcRuntime>, _>(TIMEOUT, user_provider.clone(), |x| {
                    x.vault_id == vault_id
                }),
                // the vault should execute the refund request automatically
                assert_event::<ExecuteRefundEvent<InterBtcRuntime>, _>(TIMEOUT, user_provider.clone(), |_| true),
            )
            .await;

            assert_eq!(refund_request.refund_id, refund_execution.refund_id);
            assert_eq!(refund_execution.amount, (over_payment as f64 / 1.005) as u128);
        };

        test_service(refund_service, fut_user).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_issue_overpayment_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let refund_service = vault::service::listen_for_refund_requests(vault_provider.clone(), btc_rpc.clone(), 0);

        let issue_amount = 100000;
        let over_payment_factor = 3;
        let vault_collateral =
            get_required_vault_collateral_for_issue(&vault_provider, issue_amount * over_payment_factor).await;
        assert_ok!(
            vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        let _vault_id = vault_provider.get_account_id().clone();
        let fut_user = async {
            let issue = user_provider
                .request_issue(issue_amount, vault_provider.get_account_id(), 10000)
                .await
                .unwrap();

            let metadata = btc_rpc
                .send_to_address(
                    issue.vault_btc_address,
                    (issue.amount_btc + issue.fee) as u64 * over_payment_factor as u64,
                    None,
                    0,
                )
                .await
                .unwrap();

            join(
                assert_event::<EndowedEvent<InterBtcRuntime>, _>(TIMEOUT, user_provider.clone(), |x| {
                    if &x.account_id == user_provider.get_account_id() {
                        assert_eq!(x.balance, issue.amount_btc * over_payment_factor);
                        true
                    } else {
                        false
                    }
                }),
                user_provider
                    .execute_issue(issue.issue_id, &metadata.proof, &metadata.raw_tx)
                    .map(Result::unwrap),
            )
            .await;
        };

        test_service(refund_service, fut_user).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_automatic_issue_execution_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault1_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let vault2_provider = setup_provider(client.clone(), AccountKeyring::Eve).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&vault1_provider, issue_amount).await;
        assert_ok!(
            vault1_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );
        assert_ok!(
            vault2_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        let fut_user = async {
            let issue = user_provider
                .request_issue(issue_amount, vault1_provider.get_account_id(), 10000)
                .await
                .unwrap();

            assert_ok!(
                btc_rpc
                    .send_to_address(issue.vault_btc_address, (issue.amount_btc + issue.fee) as u64, None, 0)
                    .await
            );

            // wait for vault2 to execute this issue
            let vault_id = vault1_provider.get_account_id().clone();
            assert_event::<ExecuteIssueEvent<InterBtcRuntime>, _>(TIMEOUT, user_provider.clone(), move |x| {
                x.vault_id == vault_id
            })
            .await;
        };

        let issue_set = Arc::new(IssueRequests::new());
        let (issue_event_tx, _issue_event_rx) = mpsc::channel::<CancellationEvent>(16);
        let service = join(
            vault::service::listen_for_issue_requests(
                btc_rpc.clone(),
                vault2_provider.clone(),
                issue_event_tx.clone(),
                issue_set.clone(),
            ),
            vault::service::process_issue_requests(btc_rpc.clone(), vault2_provider.clone(), issue_set.clone(), 1, 0),
        );

        test_service(service, fut_user).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_execute_open_requests_succeeds() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&vault_provider, issue_amount).await;
        assert_ok!(
            vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(&user_provider, &btc_rpc, vault_provider.get_account_id(), issue_amount).await;

        let address = BtcAddress::P2PKH(H160::from_slice(&[2; 20]));
        // place replace requests
        let redeem_ids = futures::future::join_all(
            (0..3u128).map(|_| user_provider.request_redeem(10000, address, vault_provider.get_account_id())),
        )
        .await
        .into_iter()
        .map(|x| x.unwrap())
        .collect::<Vec<_>>();

        let redeems: Vec<InterBtcRedeemRequest> =
            futures::future::join_all(redeem_ids.iter().map(|id| user_provider.get_redeem_request(id.clone())))
                .await
                .into_iter()
                .map(|x| x.unwrap())
                .collect::<Vec<_>>();

        // send btc for redeem 0
        assert_ok!(
            btc_rpc
                .send_to_address(address, redeems[0].amount_btc as u64, Some(redeem_ids[0]), 0)
                .await
        );

        let transaction = btc_rpc
            .create_transaction(address, redeems[1].amount_btc as u64, Some(redeem_ids[1]))
            .await
            .unwrap()
            .transaction;
        btc_rpc.send_to_mempool(transaction).await;

        join3(
            vault::service::execute_open_requests(vault_provider, btc_rpc.clone(), 0, Duration::from_secs(0))
                .map(Result::unwrap),
            assert_redeem_event(TIMEOUT, user_provider.clone(), redeem_ids[0]),
            assert_redeem_event(TIMEOUT, user_provider.clone(), redeem_ids[2]),
        )
        .await;

        // now move from mempool into chain and await the remaining redeem
        btc_rpc.flush_mempool().await;
        assert_redeem_event(TIMEOUT, user_provider, redeem_ids[1]).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_off_chain_liquidation() {
    test_with(|client| async move {
        let relayer_provider = setup_provider(client.clone(), AccountKeyring::Bob).await;
        let vault_provider = setup_provider(client.clone(), AccountKeyring::Charlie).await;
        let user_provider = setup_provider(client.clone(), AccountKeyring::Dave).await;

        let btc_rpc = MockBitcoinCore::new(relayer_provider.clone()).await;

        let issue_amount = 100000;
        let vault_collateral = get_required_vault_collateral_for_issue(&vault_provider, issue_amount).await;
        assert_ok!(
            vault_provider
                .register_vault(vault_collateral, btc_rpc.get_new_public_key().await.unwrap())
                .await
        );

        assert_issue(&user_provider, &btc_rpc, vault_provider.get_account_id(), issue_amount).await;

        set_exchange_rate(&relayer_provider, FixedU128::from(10)).await;

        assert_event::<LiquidateVaultEvent<InterBtcRuntime>, _>(TIMEOUT, vault_provider.clone(), |_| true).await;
    })
    .await;
}

async fn assert_redeem_event(
    duration: Duration,
    parachain_rpc: InterBtcParachain,
    redeem_id: H256,
) -> ExecuteRedeemEvent<InterBtcRuntime> {
    assert_event::<ExecuteRedeemEvent<InterBtcRuntime>, _>(duration, parachain_rpc, |x| x.redeem_id == redeem_id).await
}
