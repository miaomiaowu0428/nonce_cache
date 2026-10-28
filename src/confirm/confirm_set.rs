//! 一组签名的「集合级」确认。
//!
//! 与现有两个确认函数的区别：
//!
//! | | confirm_tx | confirm_success_tx | confirm_set |
//! |---|---|---|---|
//! | 收到首个失败 | 立刻返回 Err | 忽略，继续等成功 | 记下，继续等 |
//! | 集合内全失败 | —— | 只能等超时 | **立刻返回 AllFailed** |
//! | 时限类型 | u64 秒 | u64 秒 | Duration（可亚秒） |
//! | 返回 | 二态 | 二态 | 三态，且**每个失败都带签名** |
//!
//! 场景：多平台 / 多轮对冲。N 笔里任意一笔成交即成功，全失败才算失败，
//! 到点没等齐就返回「还差哪些（pending）+ 已经失败的（failed）」。
//!
//! # 安全契约
//!
//! **本接口不提供任何「只成交一次」保证。** 调用方必须自己在指令里放链上互斥闸
//! （典型：非幂等 create_ata）和落地窗口闸（典型：check_slot）。去掉这两条就会重复买入。

use ahash::AHashSet as HashSet;
use std::{collections::HashMap, time::Duration};

use grpc_client::TransactionFormat;
use log::{info, warn};
use solana_sdk::signature::Signature;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::{TradeStatus, TxConfirmError, tx_result_channel::TxResultEvent};

/// 失败一律带签名：`(sig, err)` 是唯一能直接 join 回调用方自己 build 表的键。
///
/// `TxConfirmError::Failed` / `MetaMissing` 内部本来也带 `signature`，
/// 但 `Other(String)` 不带，只有统一成元组，调用方才不用为每个 variant 写分支。
pub type SigErr = (Signature, TxConfirmError);

/// `confirm_set` 的三态结论。
#[derive(Clone, Debug)]
pub enum ConfirmSetOutcome {
    /// 集合内任一笔收到终态成功（首个成功的即返回，其余不再等）
    Success { sig: Signature, tx: TransactionFormat },
    /// 集合内每个签名都拿到了终态失败
    AllFailed { failed: Vec<SigErr> },
    /// 到点了还没等齐；已收到的失败仍全部带出来，方便复盘
    Timeout {
        /// 到期时**仍无终态**的签名（已失败的不会出现在这里）
        pending: Vec<Signature>,
        /// 到期前已收到终态失败的签名（带错误）
        failed: Vec<SigErr>,
    },
}

/// 在 `deadline` 内等待 `sigs` 里任意一笔终态结果。
///
/// 语义：
/// - 任一签名**成功** -> 立刻 `Success`；
/// - 每个签名都失败 -> 立刻 `AllFailed`（不等满 deadline）；
/// - `MetaMissing` 计入 failed 并继续等（与 `confirm_success_tx` 口径一致）；
/// - 集合外的签名（其它链路广播过来的）直接忽略；
/// - 广播 `Lagged` 只记 warn 不退出（容量 16384，正常不会发生；丢事件后的后果由调用方的 deadline 兜底）；
/// - 发送端全部 drop（`Closed`）-> 立刻按 `Timeout` 交回当前状态，不空转到 deadline；
/// - `sigs` 为空 -> 立刻 `AllFailed { failed: [] }`（没有可等的签名）。
///
/// 返回值里的 `pending` / `failed` 都按签名排序，便于日志对比与测试断言。
pub async fn confirm_set(
    mut rx: broadcast::Receiver<TxResultEvent>,
    sigs: HashSet<Signature>,
    deadline: Duration,
) -> ConfirmSetOutcome {
    if sigs.is_empty() {
        warn!("confirm_set: 期望集合为空，直接判定 AllFailed");
        return ConfirmSetOutcome::AllFailed { failed: Vec::new() };
    }

    info!("confirm_set: 等待 {} 个签名，deadline={:?}", sigs.len(), deadline);

    // 终态失败累积表：按签名去重（同一签名不会有两个终态，但重复事件要能吞掉）
    let mut failed: HashMap<Signature, TxConfirmError> = HashMap::new();
    let deadline_at = tokio::time::Instant::now() + deadline;
    // 用 select! 而不是 tokio::time::timeout(async {..})：后者会让协程按 move 捕获
    // failed，超时分支就拿不到已累积的失败。select! 让状态留在外层作用域。
    let sleep = tokio::time::sleep_until(deadline_at);
    tokio::pin!(sleep);
    let mut channel_closed = false;

    loop {
        tokio::select! {
            _ = &mut sleep => break,
            recv = rx.recv() => match recv {
                Ok(TxResultEvent {
                    signature,
                    tx: _,
                    status,
                }) => {
                    if !sigs.contains(&signature) {
                        // 全局广播：其它链路的交易结果，忽略
                        continue;
                    }
                    match status {
                        TradeStatus::Success { signature, tx } => {
                            info!(
                                "confirm_set: 成交 {signature}（此前已终态失败 {} 笔）",
                                failed.len()
                            );
                            return ConfirmSetOutcome::Success { sig: signature, tx };
                        }
                        TradeStatus::Failed {
                            signature,
                            tx,
                            detail,
                        } => {
                            // 注意：这里把整笔 TransactionFormat 搬进 error 了。失败数 = 变体数（几十笔）
                            // 时可以接受；要长期留存就由调用方折成 (sig, code, msg) 轻量三元组。
                            failed.insert(signature, TxConfirmError::Failed { signature, tx, detail });
                        }
                        TradeStatus::MetaMissing { signature, tx } => {
                            failed.insert(signature, TxConfirmError::MetaMissing { signature, tx });
                        }
                    }

                    // 用集合覆盖判定而不是比长度：宁可显式比对，也不依赖
                    // 「failed 的 key 恰好全是 sigs 子集」这个隐含前提。
                    if sigs.iter().all(|s| failed.contains_key(s)) {
                        info!("confirm_set: {} 个签名全部终态失败", failed.len());
                        return ConfirmSetOutcome::AllFailed {
                            failed: into_sorted_failed(failed),
                        };
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // 只记 warn，继续等：deadline 是唯一的时间边界
                    warn!("confirm_set: 广播落后 {n} 条事件（已丢弃），继续等待");
                }
                Err(RecvError::Closed) => {
                    // 发送端已全部 drop，不可能再收到任何结果：没拿到终态的按 pending 交回
                    warn!("confirm_set: 广播 channel 已关闭，按 Timeout 交回当前状态");
                    channel_closed = true;
                    break;
                }
            },
        }
    }

    let pending = into_sorted_pending(&sigs, &failed);
    warn!(
        "confirm_set: {}，pending={} failed={}",
        if channel_closed {
            "广播已关闭"
        } else {
            "到达 deadline"
        },
        pending.len(),
        failed.len()
    );
    ConfirmSetOutcome::Timeout {
        pending,
        failed: into_sorted_failed(failed),
    }
}

/// 失败列表按签名排序（`HashMap` 迭代顺序不定，日志与断言都要求稳定）
fn into_sorted_failed(failed: HashMap<Signature, TxConfirmError>) -> Vec<SigErr> {
    let mut v: Vec<SigErr> = failed.into_iter().collect();
    v.sort_by(|(a, _), (b, _)| a.as_ref().cmp(b.as_ref()));
    v
}

/// 期望集合里还没有终态的签名，按签名排序
fn into_sorted_pending(sigs: &HashSet<Signature>, failed: &HashMap<Signature, TxConfirmError>) -> Vec<Signature> {
    let mut v: Vec<Signature> = sigs.iter().copied().filter(|s| !failed.contains_key(s)).collect();
    v.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TxFailureDetail;
    use solana_sdk::transaction::{Transaction, TransactionError, VersionedTransaction};
    use std::time::Instant;

    /// 造一个占位 TransactionFormat：本函数只关心签名，不看交易内容。
    fn dummy_tx(sig: Signature) -> TransactionFormat {
        let mut tx = Transaction::default();
        tx.signatures = vec![sig];
        TransactionFormat {
            slot: 0,
            index: 0,
            meta: None,
            transaction: VersionedTransaction::from(tx),
        }
    }

    fn ev_success(sig: Signature) -> TxResultEvent {
        TxResultEvent {
            signature: sig,
            tx: dummy_tx(sig),
            status: TradeStatus::Success {
                signature: sig,
                tx: dummy_tx(sig),
            },
        }
    }

    fn ev_failed(sig: Signature) -> TxResultEvent {
        TxResultEvent {
            signature: sig,
            tx: dummy_tx(sig),
            status: TradeStatus::Failed {
                signature: sig,
                tx: dummy_tx(sig),
                detail: TxFailureDetail::new(TransactionError::AlreadyProcessed),
            },
        }
    }

    fn ev_meta_missing(sig: Signature) -> TxResultEvent {
        TxResultEvent {
            signature: sig,
            tx: dummy_tx(sig),
            status: TradeStatus::MetaMissing {
                signature: sig,
                tx: dummy_tx(sig),
            },
        }
    }

    fn set(sigs: &[Signature]) -> HashSet<Signature> {
        sigs.iter().copied().collect()
    }

    /// 本地 channel：与全局广播隔离，测试之间不会互相干扰
    fn local_channel(cap: usize) -> broadcast::Sender<TxResultEvent> {
        let (tx, _) = broadcast::channel(cap);
        tx
    }

    fn sorted(mut sigs: Vec<Signature>) -> Vec<Signature> {
        sigs.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
        sigs
    }

    /// 首个即成功：前一笔失败不影响，成功立刻返回
    #[tokio::test]
    async fn first_success_wins() {
        let (s1, s2) = (Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(64);
        let rx = tx.subscribe();
        tx.send(ev_failed(s1)).unwrap();
        tx.send(ev_success(s2)).unwrap();

        let out = confirm_set(rx, set(&[s1, s2]), Duration::from_secs(5)).await;
        match out {
            ConfirmSetOutcome::Success { sig, .. } => assert_eq!(sig, s2),
            other => panic!("期望 Success，实际 {other:?}"),
        }
    }

    /// 全失败：不等满 deadline，立刻 AllFailed，且每个签名都在
    #[tokio::test]
    async fn all_failed_returns_early() {
        let sigs = [Signature::new_unique(), Signature::new_unique(), Signature::new_unique()];
        let tx = local_channel(64);
        let rx = tx.subscribe();
        for s in sigs {
            tx.send(ev_failed(s)).unwrap();
        }

        let t0 = Instant::now();
        let out = confirm_set(rx, set(&sigs), Duration::from_secs(5)).await;
        let elapsed = t0.elapsed();
        assert!(elapsed < Duration::from_secs(1), "不应等满 deadline，实际 {elapsed:?}");

        match out {
            ConfirmSetOutcome::AllFailed { failed } => {
                assert_eq!(failed.len(), 3);
                let got: Vec<Signature> = failed.iter().map(|(s, _)| *s).collect();
                assert_eq!(got, sorted(sigs.to_vec()));
                for (_, err) in failed {
                    assert!(matches!(err, TxConfirmError::Failed { .. }));
                }
            }
            other => panic!("期望 AllFailed，实际 {other:?}"),
        }
    }

    /// 同一签名重复失败事件：按集合去重，不能把「2 个签名全失败」判成「3 笔」
    #[tokio::test]
    async fn duplicate_failure_is_deduped() {
        let (s1, s2) = (Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(64);
        let rx = tx.subscribe();
        tx.send(ev_failed(s1)).unwrap();
        tx.send(ev_failed(s1)).unwrap();
        tx.send(ev_failed(s2)).unwrap();

        let out = confirm_set(rx, set(&[s1, s2]), Duration::from_secs(5)).await;
        match out {
            ConfirmSetOutcome::AllFailed { failed } => {
                assert_eq!(failed.len(), 2);
                let got: Vec<Signature> = failed.iter().map(|(s, _)| *s).collect();
                assert_eq!(got, sorted(vec![s1, s2]));
            }
            other => panic!("期望 AllFailed，实际 {other:?}"),
        }
    }

    /// 超时：已失败的带出来，没终态的进 pending
    #[tokio::test]
    async fn timeout_splits_pending_and_failed() {
        let (s1, s2, s3) = (Signature::new_unique(), Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(64);
        let rx = tx.subscribe();
        tx.send(ev_failed(s1)).unwrap();

        let out = confirm_set(rx, set(&[s1, s2, s3]), Duration::from_millis(80)).await;
        match out {
            ConfirmSetOutcome::Timeout { pending, failed } => {
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0].0, s1);
                assert_eq!(pending, sorted(vec![s2, s3]));
            }
            other => panic!("期望 Timeout，实际 {other:?}"),
        }
    }

    /// 无关签名混入：既不触发 AllFailed，也不影响成功返回
    #[tokio::test]
    async fn unrelated_signatures_are_ignored() {
        let (mine, other) = (Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(64);
        let rx = tx.subscribe();
        // 无关签名的失败不能把我们判成 AllFailed
        tx.send(ev_failed(other)).unwrap();
        tx.send(ev_meta_missing(other)).unwrap();
        tx.send(ev_success(other)).unwrap();
        tx.send(ev_success(mine)).unwrap();

        let out = confirm_set(rx, set(&[mine]), Duration::from_secs(5)).await;
        match out {
            ConfirmSetOutcome::Success { sig, .. } => assert_eq!(sig, mine),
            other => panic!("期望 Success，实际 {other:?}"),
        }
    }

    /// 只有无关签名、自己一个都没回来 -> 必须 Timeout 而不是 AllFailed
    #[tokio::test]
    async fn unrelated_failures_do_not_make_all_failed() {
        let (mine, other) = (Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(64);
        let rx = tx.subscribe();
        for _ in 0..3 {
            tx.send(ev_failed(other)).unwrap();
        }

        let out = confirm_set(rx, set(&[mine]), Duration::from_millis(60)).await;
        match out {
            ConfirmSetOutcome::Timeout { pending, failed } => {
                assert_eq!(pending, vec![mine]);
                assert!(failed.is_empty());
            }
            other => panic!("期望 Timeout，实际 {other:?}"),
        }
    }

    /// MetaMissing 也是终态失败，但不等于「立刻 AllFailed」，还有其它签名在等
    #[tokio::test]
    async fn meta_missing_counts_as_failed_and_keeps_waiting() {
        let (s1, s2) = (Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(64);
        let rx = tx.subscribe();
        tx.send(ev_meta_missing(s1)).unwrap();
        tx.send(ev_success(s2)).unwrap();

        let out = confirm_set(rx, set(&[s1, s2]), Duration::from_secs(5)).await;
        match out {
            ConfirmSetOutcome::Success { sig, .. } => assert_eq!(sig, s2),
            other => panic!("期望 Success，实际 {other:?}"),
        }
    }

    /// 集合只有一个签名时，它一失败就是 AllFailed：不等后续事件。
    /// 链上终态唯一，同一签名不会先失败再成功，所以这里不引入「等待重复事件」的复杂度。
    #[tokio::test]
    async fn single_sig_failure_is_immediate_all_failed() {
        let s1 = Signature::new_unique();
        let tx = local_channel(64);
        let rx = tx.subscribe();
        tx.send(ev_failed(s1)).unwrap();

        let t0 = Instant::now();
        let out = confirm_set(rx, set(&[s1]), Duration::from_secs(5)).await;
        assert!(t0.elapsed() < Duration::from_secs(1));
        match out {
            ConfirmSetOutcome::AllFailed { failed } => {
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0].0, s1);
            }
            other => panic!("期望 AllFailed，实际 {other:?}"),
        }
    }

    /// Lagged：容量打满导致丢事件，不能 break，后续事件仍要能读到成功
    #[tokio::test]
    async fn lagged_is_tolerated() {
        let (mine, other) = (Signature::new_unique(), Signature::new_unique());
        let tx = local_channel(2); // 故意极小，制造 Lagged
        let rx = tx.subscribe();
        for _ in 0..10 {
            tx.send(ev_failed(other)).unwrap();
        }
        tx.send(ev_success(mine)).unwrap();

        let out = confirm_set(rx, set(&[mine, other]), Duration::from_secs(5)).await;
        match out {
            // 丢掉的无关事件不影响结论；我们的成功事件仍在缓冲里
            ConfirmSetOutcome::Success { sig, .. } => assert_eq!(sig, mine),
            other => panic!("期望 Success，实际 {other:?}"),
        }
    }

    /// 发送端全部 drop：立刻按 Timeout 交回，不空转到 deadline
    #[tokio::test]
    async fn closed_channel_returns_immediately() {
        let mine = Signature::new_unique();
        let tx = local_channel(8);
        let rx = tx.subscribe();
        drop(tx);

        let t0 = Instant::now();
        let out = confirm_set(rx, set(&[mine]), Duration::from_secs(30)).await;
        assert!(t0.elapsed() < Duration::from_secs(1));
        match out {
            ConfirmSetOutcome::Timeout { pending, failed } => {
                assert_eq!(pending, vec![mine]);
                assert!(failed.is_empty());
            }
            other => panic!("期望 Timeout，实际 {other:?}"),
        }
    }

    /// 空集合：立刻 AllFailed，不空等
    #[tokio::test]
    async fn empty_set_returns_all_failed() {
        let tx = local_channel(8);
        let rx = tx.subscribe();
        let out = confirm_set(rx, HashSet::default(), Duration::from_secs(30)).await;
        match out {
            ConfirmSetOutcome::AllFailed { failed } => assert!(failed.is_empty()),
            other => panic!("期望 AllFailed，实际 {other:?}"),
        }
    }

    /// 走真实的全局广播：证明 tx_result_channel::subscribe/send 这条线是对的
    #[tokio::test]
    async fn works_with_global_broadcast() {
        let sig = Signature::new_unique();
        let rx = crate::tx_result_channel::subscribe();
        // 先注册再发送（与线上调用顺序一致）
        crate::tx_result_channel::send(ev_success(sig)).unwrap();

        let out = confirm_set(rx, set(&[sig]), Duration::from_secs(5)).await;
        match out {
            ConfirmSetOutcome::Success { sig: got, .. } => assert_eq!(got, sig),
            other => panic!("期望 Success，实际 {other:?}"),
        }
    }
}
