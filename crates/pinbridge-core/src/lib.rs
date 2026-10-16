//! PinBridge 领域模型。
//!
//! 这一层刻意"零副作用、零平台依赖"：它不碰 Pin、不碰网络、不碰文件系统，
//! 只描述"谁在什么会话里、拿着什么权限、对目标做了什么"。
//!
//! 之所以把它单独拆出来，是因为 HubService、MCP 工具层、CLI、以及注入进
//! 目标进程的 agent 都要对同一组概念达成一致；一旦这些类型散落在各 crate，
//! 权限判断就会出现"两处实现、两处漏判"的经典问题。
//!
//! 设计要点（对应 docs/architecture/control-ownership.md）：
//!
//! 1. **人与 AI 平级**：二者都是 [`Principal`]，不存在"AI 只能通过人类代理"的层级。
//! 2. **控制权是显式租约**：[`ControlLease`] 带 epoch，任何控制命令都必须携带
//!    签发时的 epoch；旧 epoch 的命令会被拒绝，避免"AI 写了一半的脚本被人类接管后
//!    仍然生效"。
//! 3. **能力集只能缩小**：[`CapabilitySet`] 一旦被 `intersect`，结果永远是其子集，
//!    agent 侧再校验一次（双端校验，任何一端被绕过都不至于失控）。
//! 4. **事件通道有界且可观测**：背压不可协商，宁可丢弃并计数，也不让被分析进程因
//!    为分析工具而 OOM。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
pub mod channel;
pub mod error;
pub mod id;
pub mod principal;
pub mod session;
pub mod time;

pub use capability::{Capability, CapabilitySet, ControlLease, LeaseError};
pub use channel::{bounded, BoundedReceiver, BoundedSender, DropPolicy};
pub use error::{Error, Result};
pub use id::{AgentId, EventSeq, ProcessId, ScriptId, SessionId, ThreadId};
pub use principal::{Principal, PrincipalKind};
pub use session::{Session, SessionState, TargetSpec};
pub use time::{now_unix_millis, Duration, Instant};

/// 协议与轨迹格式的版本号。任何破坏性变更都必须同时提升这个常量，
/// 并在 `docs/specs/` 下更新对应规范（CI 会检查 CHANGELOG 含有条目）。
pub const WIRE_VERSION: u16 = 1;

/// 全项目统一的 magic 前缀，用于 .pbt 文件头与 IPC 握手。
pub const MAGIC: [u8; 4] = *b"PBT1";

/// agent 侧单次脚本执行的时间预算上限（毫秒）。
///
/// 这不是"性能优化"，而是安全阀：agent 跑在被分析进程里，脚本里的死循环
/// 会直接冻结目标。超时后脚本被标记为 `TimedOut` 并强制卸载，
/// 详情见 `docs/design/script-runtime.md`。
pub const SCRIPT_TIME_BUDGET_MS: u64 = 5_000;

/// 有界事件通道的默认容量（事件条数）。
pub const DEFAULT_CHANNEL_CAPACITY: usize = 65_536;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[test]
    fn channel_is_bounded_and_counts_drops() {
        let (tx, rx) = bounded::<u32>(4, DropPolicy::DropNewest);
        for i in 0..10 {
            let _ = tx.send(i);
        }
        assert_eq!(tx.dropped(), 6, "容量 4 的通道塞 10 条应丢弃 6 条");
        assert_eq!(rx.len(), 4);
        tx.close();
        let drained: Vec<u32> = rx.drain().collect();
        assert_eq!(drained, vec![0, 1, 2, 3], "DropNewest 保留最早写入的 4 条");
    }

    #[test]
    fn lease_rejects_stale_epoch() {
        let mut lease =
            ControlLease::grant(Principal::human("alice"), 1, StdDuration::from_secs(30));
        assert!(lease.accepts(1));
        lease.revoke();
        lease.renew(Principal::ai("agent-7"), 2);
        assert!(!lease.accepts(1), "旧 epoch 的控制命令必须被拒绝");
        assert!(lease.accepts(2));
        assert_eq!(lease.holder().kind(), PrincipalKind::Ai);
    }

    #[test]
    fn capability_set_can_only_shrink() {
        let wide = CapabilitySet::from_iter([Capability::MemRead, Capability::MemWrite]);
        let narrow = wide.intersect(CapabilitySet::from_iter([Capability::MemRead]));
        assert!(narrow.contains(Capability::MemRead));
        assert!(!narrow.contains(Capability::MemWrite));
        assert!(narrow.intersect(wide).is_subset_of(narrow));
    }

    #[test]
    fn empty_capability_set_denies_everything() {
        let err = CapabilitySet::empty()
            .require(Capability::MemRead)
            .unwrap_err();
        assert!(matches!(err, Error::CapabilityDenied { .. }));
    }

    #[test]
    fn expire_marks_lease_dead() {
        let mut lease = ControlLease::grant(Principal::ai("a"), 5, StdDuration::from_millis(0));
        lease.expire_if_stale(now_unix_millis());
        assert!(!lease.is_active());
    }
}
