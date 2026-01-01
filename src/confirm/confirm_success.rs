use std::collections::HashSet;

use grpc_client::TransactionFormat;
use log::{error, info};
use solana_sdk::signature::Signature;

use crate::{TradeStatus, tx_result_channel::TxResultEvent};

pub async fn confirm_success_tx(
    mut tx_result_rx: tokio::sync::broadcast::Receiver<TxResultEvent>,
    expected_signatures: HashSet<Signature>,
    timeout_secs: u64,
) -> Result<(Signature, TransactionFormat), Box<dyn std::error::Error + Sync + Send>> {
    let res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            if let Ok(TxResultEvent {
                signature: sig,
                tx,
                status,
            }) = tx_result_rx.recv().await
            {
                if expected_signatures.contains(&sig) {
                    info!("交易确认: {:?} -> {:#?}", sig, status);
                    match status {
                        TradeStatus::Success(_) => return Ok((sig, tx)),
                        TradeStatus::Failed(_, _) => {
                            // 只记录失败，但继续等待其他交易的成功
                            error!("交易失败: {:?}，继续等待其他交易", sig);
                            continue;
                        }
                    }
                } else {
                    // 广播模式下，直接忽略不属于我们的交易结果
                    info!("非本组交易, 忽略: {:?}", sig);
                }
            }
        }
    })
    .await;

    match res {
        Ok(Ok((sig, tx))) => Ok((sig, tx)),
        Ok(Err(e)) => {
            error!("this should not happen, a loop should not return Err");
            Err(e)
        }
        Err(_) => Err(format!("所有交易都失败或超时").into()),
    }
}
