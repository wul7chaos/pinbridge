//! 能力集与控制权租约。
//!
//! 两件事必须分清：
//!
//! * **能力集（[`CapabilitySet`]）**：某个主体"被允许做什么"。只能收敛，不能扩张。
//! * **控制权租约（[`ControlLease`]）**：此刻"谁说了算"。带 epoch 与过期时间。
//!
//! 为什么要带 epoch：AI 常常是长任务。假设 AI 正在加载一套 hook 脚本，人类中途
//! 按了"接管"并手工改写了内存；如果 AI 的在途命令没有 epoch 校验，它下一秒就会
//! 把人类刚建立的现场覆盖掉，而两边都认为自己在正常工作。epoch 让在途命令失效，
//! 冲突变成显式错误而不是静默破坏。

use std::time::Duration as StdDuration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::principal::Principal;
use crate::time::now_unix_millis;

/// 单项能力。命名刻意跟"动作"而非"对象"对齐，便于审计日志直接读懂。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 读取目标内存 / 寄存器。
    MemRead,
    /// 写入目标内存。
    MemWrite,
    /// 改写寄存器上下文（含 PC/SP）。
    RegWrite,
    /// 下断点与删除断点。
    Breakpoint,
    /// 单步执行。
    SingleStep,
    /// 系统调用 Hook。
    SyscallHook,
    /// 指令模板匹配 Hook。
    PatternHook,
    /// Windows 异常时机接管。
    ExceptionSteer,
    /// 录制/导出执行轨迹（可能包含敏感内存内容）。
    TraceRecord,
    /// 启停目标进程、注入/卸载 agent。
    ProcessControl,
    /// 在目标进程内加载或替换 Python 脚本。这是最强的一档能力。
    ScriptLoad,
    /// 授予/收回他人的能力（只有人类可持有）。
    CapabilityGrant,
}

impl Capability {
    /// 全部能力，按"最小惊讶"顺序排列，供 CLI/工作台渲染授权矩阵。
    pub const ALL: [Self; 12] = [
        Self::MemRead,
        Self::MemWrite,
        Self::RegWrite,
        Self::Breakpoint,
        Self::SingleStep,
        Self::SyscallHook,
        Self::PatternHook,
        Self::ExceptionSteer,
        Self::TraceRecord,
        Self::ProcessControl,
        Self::ScriptLoad,
        Self::CapabilityGrant,
    ];

    /// 只读观察用的能力集：给"观察员"和默认的 AI 只读会话。
    ///
    /// 注意这里必须用 [`Capability::bit`] 而不是 `Self::X as u32`：
    /// 判别值（discriminant）是 0..11 的**序号**，不是位掩码。
    /// 把序号当掩码用会得到一个看似合理、实际完全错误的能力集
    /// （例如 `TraceRecord` 的序号是 8，被当成 `1 << 3 = Breakpoint`）。
    #[must_use]
    pub const fn read_only_baseline() -> CapabilitySet {
        CapabilitySet::from_bits(Self::MemRead.bit() | Self::TraceRecord.bit())
    }

    /// 人类操作员的默认能力集（不含 `CapabilityGrant` 之外的全部）。
    #[must_use]
    pub const fn operator_default() -> CapabilitySet {
        CapabilitySet::from_bits((1u32 << Self::ALL.len()) - 1)
    }

    /// 稳定的位索引，用于 IPC 序列化（不要依赖 `as u32`，将来可能重排）。
    #[must_use]
    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// 稳定短名。会出现在 JSON、CLI 输出与审计日志里——
    /// **改动即为破坏性变更**，必须走协议版本升级流程。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MemRead => "mem_read",
            Self::MemWrite => "mem_write",
            Self::RegWrite => "reg_write",
            Self::Breakpoint => "breakpoint",
            Self::SingleStep => "single_step",
            Self::SyscallHook => "syscall_hook",
            Self::PatternHook => "pattern_hook",
            Self::ExceptionSteer => "exception_steer",
            Self::TraceRecord => "trace_record",
            Self::ProcessControl => "process_control",
            Self::ScriptLoad => "script_load",
            Self::CapabilityGrant => "capability_grant",
        }
    }
}

/// 能力集。位图实现，保证 `intersect` 结果一定是子集。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CapabilitySet(u32);

impl CapabilitySet {
    /// 空集。
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// 由位图构造。超出 [`Capability::ALL`] 宽度的高位会被丢弃。
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        let mask = (1u32 << Capability::ALL.len()) - 1;
        Self(bits & mask)
    }

    /// 取出位图。
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 是否包含某项能力。
    #[must_use]
    pub const fn contains(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    /// 加入能力（内部用；对外必须走人类授权路径）。
    #[must_use]
    pub const fn with(self, cap: Capability) -> Self {
        Self::from_bits(self.0 | cap.bit())
    }

    /// 去掉能力。
    #[must_use]
    pub const fn without(self, cap: Capability) -> Self {
        Self::from_bits(self.0 & !cap.bit())
    }

    /// 交集。这是唯一允许的"合并"方式——权限只会变小。
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// 是否为 `other` 的子集。
    #[must_use]
    pub const fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 缺少某项能力时返回可读错误（agent 侧与 hub 侧共用同一份判断）。
    pub fn require(self, cap: Capability) -> Result<()> {
        if self.contains(cap) {
            Ok(())
        } else {
            Err(Error::CapabilityDenied {
                needed: cap,
                held: self,
            })
        }
    }

    /// 迭代已授予的能力。
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .into_iter()
            .filter(move |cap| self.contains(*cap))
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        iter.into_iter().fold(Self::empty(), Self::with)
    }
}

/// 序列化为 `{"bits": 4095, "names": ["mem_read", ...]}`。
///
/// 为什么同时给位图和名字：位图让程序可靠解析，名字让 AI 与人类在
/// `tool_result` 里一眼看出"我现在有什么权限"，不需要查文档对照表。
impl Serialize for CapabilitySet {
    fn serialize<S: Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("CapabilitySet", 2)?;
        s.serialize_field("bits", &self.0)?;
        s.serialize_field(
            "names",
            &self.iter().map(Capability::name).collect::<Vec<_>>(),
        )?;
        s.end()
    }
}

/// 反序列化接受两种输入：裸位图整数，或带名字的对象。
/// 前者便于压缩传输，后者便于人类手写配置。
#[derive(Deserialize)]
#[serde(untagged)]
enum CapabilitySetRepr {
    Bits(u32),
    Named { bits: u32 },
}

impl<'de> Deserialize<'de> for CapabilitySet {
    fn deserialize<D: Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        Ok(match CapabilitySetRepr::deserialize(de)? {
            CapabilitySetRepr::Bits(bits) => Self::from_bits(bits),
            CapabilitySetRepr::Named { bits } => Self::from_bits(bits),
        })
    }
}

/// 控制权租约的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseState {
    Active,
    Revoked,
    Expired,
}

/// 控制权租约。
///
/// * `epoch` 单调递增，只有 `renew` 会推进它；
/// * `expires_at_ms` 让"AI 执行到一半就崩了"不会永久占住控制权（默认由 hub 续租）；
/// * `revoke` 是人类的硬接管，AI 下一次提交命令时才会知道自己被踢出局。
#[derive(Debug, Clone)]
pub struct ControlLease {
    holder: Principal,
    capabilities: CapabilitySet,
    epoch: u64,
    expires_at_ms: u64,
    state: LeaseState,
    /// 续租窗口（毫秒），每次 `renew` 重置。
    ttl_ms: u64,
}

/// 租约操作失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// 租约已不在有效期内。
    #[error("租约已失效（revoked={revoked}, expired={expired}）")]
    Inactive {
        /// 是否被显式收回。
        revoked: bool,
        /// 是否超时。
        expired: bool,
    },
}

impl ControlLease {
    /// 授予一份租约。`ttl` 为 0 表示"立即过期"，仅用于测试与硬接管后的占位。
    #[must_use]
    pub fn grant(holder: Principal, epoch: u64, ttl: StdDuration) -> Self {
        Self {
            holder,
            capabilities: CapabilitySet::empty(),
            epoch,
            expires_at_ms: now_unix_millis().saturating_add(ttl.as_millis() as u64),
            state: LeaseState::Active,
            ttl_ms: ttl.as_millis() as u64,
        }
    }

    /// 附带能力集（链式，便于在会话建立时一次性写清）。
    #[must_use]
    pub fn with_capabilities(mut self, caps: CapabilitySet) -> Self {
        self.capabilities = caps;
        self
    }

    /// 当前持有者。
    #[must_use]
    pub const fn holder(&self) -> &Principal {
        &self.holder
    }

    /// 当前 epoch。
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 租约附带的能力集。
    #[must_use]
    pub const fn capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    /// 是否仍然有效（含惰性过期判定）。
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self.state, LeaseState::Active) && now_unix_millis() < self.expires_at_ms
    }

    /// 校验一条在途命令携带的 epoch 是否仍然有效。
    #[must_use]
    pub fn accepts(&self, requested_epoch: u64) -> bool {
        self.state == LeaseState::Active && requested_epoch == self.epoch
    }

    /// 人类硬接管：收回租约。
    pub fn revoke(&mut self) {
        self.state = LeaseState::Revoked;
    }

    /// 惰性过期检查，由 hub 的心跳调用。
    pub fn expire_if_stale(&mut self, now_ms: u64) {
        if self.state == LeaseState::Active && now_ms >= self.expires_at_ms {
            self.state = LeaseState::Expired;
        }
    }

    /// 移交/续租：推进 epoch，原持有者的在途命令立即失效。
    pub fn renew(&mut self, holder: Principal, new_epoch: u64) -> u64 {
        self.holder = holder;
        self.epoch = new_epoch;
        self.state = LeaseState::Active;
        self.expires_at_ms = now_unix_millis().saturating_add(self.ttl_ms);
        self.epoch
    }

    /// 在租约内校验能力，返回的是"租约能力 ∩ 请求方能力"的判定结果。
    pub fn ensure(&self, cap: Capability) -> Result<()> {
        if !self.is_active() {
            return Err(Error::ControlConflict {
                holder: self.holder.to_string(),
                current_epoch: self.epoch,
                requested_epoch: self.epoch,
            });
        }
        self.capabilities.require(cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_default_covers_all_except_none() {
        let caps = Capability::operator_default();
        for cap in Capability::ALL {
            assert!(caps.contains(cap), "{cap:?} 应在操作员默认能力集内");
        }
        assert_eq!(caps, CapabilitySet::from_iter(Capability::ALL));
    }

    #[test]
    fn read_only_baseline_is_strictly_narrower() {
        let ro = Capability::read_only_baseline();
        assert!(ro.is_subset_of(Capability::operator_default()));
        assert!(ro.contains(Capability::MemRead), "只读基线必须能读内存");
        assert!(ro.contains(Capability::TraceRecord), "只读基线必须能读轨迹");
        assert_eq!(
            ro.bits(),
            Capability::MemRead.bit() | Capability::TraceRecord.bit(),
            "位掩码必须由 bit() 计算，不能拿判别值当掩码"
        );
        assert!(!ro.contains(Capability::MemWrite));
        assert!(!ro.contains(Capability::Breakpoint));
        assert!(!ro.contains(Capability::ScriptLoad));
    }

    #[test]
    fn every_capability_bit_is_distinct_and_within_bounds() {
        let mut seen = 0u32;
        for cap in Capability::ALL {
            assert_eq!(cap.bit() & seen, 0, "{cap:?} 的位与其它能力冲突");
            seen |= cap.bit();
        }
        assert_eq!(seen, Capability::operator_default().bits());
    }

    #[test]
    fn capability_denied_reports_missing_bit() {
        let err = Capability::read_only_baseline()
            .require(Capability::MemWrite)
            .unwrap_err();
        match err {
            Error::CapabilityDenied { needed, held } => {
                assert_eq!(needed, Capability::MemWrite);
                assert!(!held.contains(Capability::MemWrite));
            }
            other => panic!("期望 CapabilityDenied，实际 {other:?}"),
        }
    }

    #[test]
    fn high_bits_are_masked_out() {
        assert_eq!(
            CapabilitySet::from_bits(u32::MAX),
            Capability::operator_default()
        );
    }

    #[test]
    fn inactive_lease_refuses_capability() {
        let lease = ControlLease::grant(Principal::ai("a"), 1, StdDuration::from_millis(0))
            .with_capabilities(Capability::operator_default());
        assert!(matches!(
            lease.ensure(Capability::MemRead),
            Err(Error::ControlConflict { .. })
        ));
    }

    #[test]
    fn capability_names_are_unique_and_stable() {
        let names: Vec<&str> = Capability::ALL
            .iter()
            .copied()
            .map(Capability::name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "能力短名必须唯一");
        assert_eq!(names[0], "mem_read");
        assert_eq!(names[names.len() - 1], "capability_grant");
    }

    #[test]
    fn serde_roundtrip_keeps_bits_and_names() {
        let caps = CapabilitySet::from_iter([Capability::MemRead, Capability::ScriptLoad]);
        let text = serde_json::to_string(&caps).unwrap();
        assert!(text.contains("\"mem_read\""), "实际: {text}");
        assert!(text.contains("\"script_load\""));
        assert_eq!(serde_json::from_str::<CapabilitySet>(&text).unwrap(), caps);
        // 也接受裸位图
        assert_eq!(
            serde_json::from_str::<CapabilitySet>("3").unwrap(),
            CapabilitySet::from_bits(3)
        );
    }
}
