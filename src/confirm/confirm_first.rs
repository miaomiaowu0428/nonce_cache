use std::collections::HashSet;

use grpc_client::TransactionFormat;
use log::{error, info};
use solana_sdk::signature::Signature;

use crate::{TxConfirmError, TradeStatus, tx_result_channel::TxResultEvent};

/// 监听交易结果的通用函数
///
/// # 参数
/// - `tx_result_rx`: 已订阅的交易结果接收端
/// - `expected_signatures`: 期望的交易签名集合
/// - `timeout_secs`: 超时时间（秒）
///
/// # 返回
/// - `Ok((Signature, TransactionFormat))`: 成功获取到交易签名和内容
/// - `Err(TxConfirmError)`: 详细的错误信息（超时/失败/Meta缺失等）
pub async fn confirm_tx(
    mut tx_result_rx: tokio::sync::broadcast::Receiver<TxResultEvent>,
    expected_signatures: HashSet<Signature>,
    timeout_secs: u64,
) -> Result<(Signature, TransactionFormat), TxConfirmError> {
    info!("confirming: {expected_signatures:#?}");
    let res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            if let Ok(TxResultEvent {
                signature,
                tx: _,
                status,
            }) = tx_result_rx.recv().await
            {
                if expected_signatures.contains(&signature) {
                    info!("交易确认: {:?} -> {:#?}", signature, status);
                    match status {
                        TradeStatus::Success { signature, tx } => return Ok((signature, tx)),
                        TradeStatus::Failed { signature, tx, error_msg } => {
                            error!("交易失败: {:?} - {}", signature, error_msg);
                            return Err(TxConfirmError::Failed {
                                signature,
                                tx,
                                error_msg,
                            });
                        }
                        TradeStatus::MetaMissing { signature, tx } => {
                            error!("交易 Meta 缺失: {:?}", signature);
                            return Err(TxConfirmError::MetaMissing { signature, tx });
                        }
                    }
                } else {
                    // 广播模式下，直接忽略不属于我们的交易结果
                    info!("非本组交易, 忽略: {:?}", signature);
                }
            }
        }
    })
    .await;

    match res {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(TxConfirmError::Timeout {
            expected_sigs: expected_signatures.into_iter().collect(),
            timeout_secs,
        }),
    }
}
