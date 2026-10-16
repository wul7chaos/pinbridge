//! 操作溯源（provenance）。
//!
//! 存在的理由很具体：当 AI 说"我把 0x401000 的校验跳过去了"，三个小时后
//! 内存里出现了别的值，人和 AI 谁都说不清是谁改的。溯源日志回答的正是这个问题：
//! **谁、在什么时间、用哪一版脚本、对哪个会话、做了什么、结果如何**。
//!
//! 设计要求：
//!
//! * 只追加，不修改（追加式存储让"日志被改写"这件事本身可被检测）；
//! * 每条记录自带 `seq`，跨进程/跨重启可比对；
//! * 记录里带脚本哈希而不是脚本正文——正文可能很大，哈希足以定位版本；
//! * 记录同时写进 `.pbt` 的 AUDIT 段，轨迹与审计不会脱节。

use serde::{Deserialize, Serialize};

use pinbridge_core::principal::Principal;
use pinbridge_core::time::{format_unix_millis_utc, now_unix_millis};

/// 一条审计记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 单调序号，从 1 开始。
    pub seq: u64,
    /// 发生时间（Unix 毫秒）。
    pub at_ms: u64,
    /// 人类可读时间（UTC），避免每次看日志都要换算。
    pub at: String,
    /// 所属会话。Hub 级操作（建会话前的授权等）为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u64>,
    /// 主体（`human:alice` / `ai:agent-7`）。
    pub actor: Principal,
    /// 动作名，建议用 `动词_对象` 形式：`load_script` / `control_pause` / `grant_capabilities`。
    pub action: String,
    /// 附加细节（脚本哈希、断点地址、epoch……）。自由字段但必须是可读文本。
    pub detail: String,
    /// 结果：`ok` / 错误摘要。
    pub outcome: String,
}

/// 只追加的溯源日志。
#[derive(Debug, Default)]
pub struct ProvenanceLog {
    entries: Vec<AuditEntry>,
    next_seq: u64,
}

impl ProvenanceLog {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条"Hub 级"成功记录（不属于具体会话）。
    pub fn record_ok(
        &mut self,
        actor: &Principal,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> u64 {
        self.append(None, actor, action.into(), detail.into(), "ok".to_owned())
    }

    /// 追加一条"会话级"成功记录。
    pub fn record_ok_in(
        &mut self,
        session: u64,
        actor: &Principal,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> u64 {
        self.append(
            Some(session),
            actor,
            action.into(),
            detail.into(),
            "ok".to_owned(),
        )
    }

    /// 追加一条"Hub 级"失败记录。**失败也要留痕**：AI 的失败尝试是理解它
    /// "为什么走到了当前策略"的关键线索。
    pub fn record_failure(
        &mut self,
        actor: &Principal,
        action: impl Into<String>,
        detail: impl Into<String>,
        outcome: impl Into<String>,
    ) -> u64 {
        self.append(None, actor, action.into(), detail.into(), outcome.into())
    }

    /// 追加一条"会话级"失败记录。
    pub fn record_failure_in(
        &mut self,
        session: u64,
        actor: &Principal,
        action: impl Into<String>,
        detail: impl Into<String>,
        outcome: impl Into<String>,
    ) -> u64 {
        self.append(
            Some(session),
            actor,
            action.into(),
            detail.into(),
            outcome.into(),
        )
    }

    fn append(
        &mut self,
        session: Option<u64>,
        actor: &Principal,
        action: String,
        detail: String,
        outcome: String,
    ) -> u64 {
        self.next_seq += 1;
        let at_ms = now_unix_millis();
        self.entries.push(AuditEntry {
            seq: self.next_seq,
            at_ms,
            at: format_unix_millis_utc(at_ms),
            session,
            actor: actor.clone(),
            action,
            detail,
            outcome,
        });
        self.next_seq
    }

    /// 全部记录。
    #[must_use]
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// 记录条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 序列化为 JSON Lines（每行一条），供 `.pbt` 的 AUDIT 段与 CLI 输出使用。
    pub fn to_json_lines(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect()
    }

    /// 只保留某个主体的记录（"AI 到底做了什么"的快速视图）。
    #[must_use]
    pub fn by_actor(&self, actor: &Principal) -> Vec<&AuditEntry> {
        self.entries
            .iter()
            .filter(|e| e.actor.same_as(actor))
            .collect()
    }

    /// 某个会话的全部记录（含失败尝试），按发生顺序。
    #[must_use]
    pub fn for_session(&self, session: u64) -> Vec<&AuditEntry> {
        self.entries
            .iter()
            .filter(|e| e.session == Some(session))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_is_monotonic_and_actors_are_filterable() {
        let mut log = ProvenanceLog::new();
        let ai = Principal::ai("agent-7");
        let human = Principal::human("alice");
        assert_eq!(log.record_ok_in(1, &ai, "load_script", "sha256=ab"), 1);
        assert_eq!(log.record_ok_in(1, &human, "control_pause", "epoch=2"), 2);
        assert_eq!(
            log.record_failure_in(1, &ai, "memory_write", "0x1000", "能力不足"),
            3
        );
        assert_eq!(log.record_ok(&human, "create_session", "TARGET.exe"), 4);
        assert_eq!(log.len(), 4);
        assert_eq!(log.by_actor(&ai).len(), 2);
        assert_eq!(log.for_session(1).len(), 3, "Hub 级记录不应计入会话视图");
        assert_eq!(log.entries()[2].outcome, "能力不足");
        assert!(!log.entries()[0].at.is_empty());
        assert!(log.entries()[3].session.is_none());
    }

    #[test]
    fn json_lines_are_one_entry_per_line() {
        let mut log = ProvenanceLog::new();
        log.record_ok(&Principal::ai("a"), "act", "detail");
        log.record_ok(&Principal::ai("a"), "act2", "detail2");
        let lines = log.to_json_lines();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert_eq!(line.lines().count(), 1);
            let parsed: AuditEntry = serde_json::from_str(line).unwrap();
            assert!(parsed.seq > 0);
        }
    }
}
