use std::collections::HashSet;

use grpc_client::TransactionFormat;
use log::{error, info};
use solana_sdk::signature::Signature;

use crate::{TradeStatus, TxConfirmError, tx_result_channel::TxResultEvent};

/// 监听交易结果的函数，只关心成功的交易
/// 失败的交易会被忽略，继续等待其他交易
///
/// # 参数
/// - `tx_result_rx`: 已订阅的交易结果接收端
/// - `expected_signatures`: 期望的交易签名集合
/// - `timeout_secs`: 超时时间（秒）
///
/// # 返回
/// - `Ok((Signature, TransactionFormat))`: 成功获取到交易签名和内容
/// - `Err(TxConfirmError)`: 详细的错误信息（超时）
pub async fn confirm_success_tx(
    mut tx_result_rx: tokio::sync::broadcast::Receiver<TxResultEvent>,
    expected_signatures: HashSet<Signature>,
    timeout_secs: u64,
) -> Result<(Signature, TransactionFormat), TxConfirmError> {
    let res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            if let Ok(TxResultEvent {
                signature: sig,
                tx: _,
                status,
            }) = tx_result_rx.recv().await
            {
                if expected_signatures.contains(&sig) {
                    info!("交易确认: {:?} -> {:#?}", sig, status);
                    match status {
                        TradeStatus::Success { signature, tx } => return Ok((signature, tx)),
                        TradeStatus::Failed {
                            signature,
                            error_msg,
                            ..
                        } => {
                            // 只记录失败，但继续等待其他交易的成功
                            error!(
                                "交易失败: {:?} - {}，继续等待其他交易",
                                signature, error_msg
                            );
                            continue;
                        }
                        TradeStatus::MetaMissing { signature, .. } => {
                            // Meta 缺失也当作失败，继续等待
                            error!("交易 Meta 缺失: {:?}，继续等待其他交易", signature);
                            continue;
                        }
                    }
                } else {
                    // 广播模式下，直接忽略不属于我们的交易结果
                    // info!("非本组交易, 忽略: {:?}", sig);
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
        Err(_) => Err(TxConfirmError::Timeout {
            expected_sigs: expected_signatures.into_iter().collect(),
            timeout_secs,
        }),
    }
}
