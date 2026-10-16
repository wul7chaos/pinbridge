//! 统一错误类型。
//!
//! 原则：错误必须"可归因"——能回答"是权限不够、目标不在、还是通道满了"。
//! 因为控制链路上任何一次静默失败，都会在对抗场景里表现为
//! "分析结果看起来合理但其实是错的"，这比直接报错更危险。

use thiserror::Error;

/// PinBridge 的库级错误。
#[derive(Debug, Error)]
pub enum Error {
    /// 目标进程/会话相关的问题。
    #[error("目标不可用: {0}")]
    TargetUnavailable(String),

    /// 权限不足。`needed` 是进程内校验时缺的那个位。
    #[error("能力不足: 需要 {needed:?}，当前持有 {held:?}")]
    CapabilityDenied {
        /// 缺失的能力。
        needed: super::Capability,
        /// 实际持有的能力集（便于直接出现在 AI 的 tool_result 里）。
        held: super::CapabilitySet,
    },

    /// 控制权不在调用方手里，或 epoch 过期。
    #[error(
        "控制权冲突: 当前持有者 {holder}，epoch {current_epoch}，请求 epoch {requested_epoch}"
    )]
    ControlConflict {
        /// 当前租约持有者的显示名。
        holder: String,
        /// 当前 epoch。
        current_epoch: u64,
        /// 请求方携带的 epoch。
        requested_epoch: u64,
    },

    /// 脚本生命周期错误（加载失败、被替换、超时）。
    #[error("脚本错误: {0}")]
    Script(String),

    /// 事件通道满且策略为拒绝写入。
    #[error("事件通道已满: 容量 {capacity}，丢弃策略 {policy:?}")]
    ChannelFull {
        /// 通道容量。
        capacity: usize,
        /// 当前丢弃策略。
        policy: super::DropPolicy,
    },

    /// 轨迹读写错误。
    #[error("轨迹错误: {0}")]
    Trace(String),

    /// 编解码/协议错误。
    #[error("协议错误: {0}")]
    Protocol(String),

    /// I/O 透传。
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 项目统一 `Result`。
pub type Result<T> = std::result::Result<T, Error>;
