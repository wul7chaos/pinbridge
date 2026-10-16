//! 主体（Principal）：谁在操作。
//!
//! 人与 AI 是**平级**的两种主体。这是 PinBridge 与"调试器 + MCP 插件"方案的
//! 根本分歧：插件方案里 AI 永远是人类的附庸，主导权在人手里；PinBridge 认为
//! AI 可以独立持有控制权、独立加载脚本，人类默认只是观察员，必要时才接管。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 主体类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// 人类分析员。
    Human,
    /// AI 客户端 / agent。
    Ai,
    /// 系统自身（定时器、收尾流程等）。不计入权限审计的"人"。
    System,
}

impl PrincipalKind {
    /// 是否为外部主体（需要走权限校验）。
    #[must_use]
    pub const fn is_external(self) -> bool {
        matches!(self, Self::Human | Self::Ai)
    }

    /// 审计日志里的短名。
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Ai => "ai",
            Self::System => "system",
        }
    }
}

impl fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tag())
    }
}

/// 一个具体主体。`name` 只用于展示与审计，不参与鉴权。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Principal {
    kind: PrincipalKind,
    name: String,
}

impl Principal {
    /// 人类分析员。
    #[must_use]
    pub fn human(name: impl Into<String>) -> Self {
        Self {
            kind: PrincipalKind::Human,
            name: name.into(),
        }
    }

    /// AI 客户端。
    #[must_use]
    pub fn ai(name: impl Into<String>) -> Self {
        Self {
            kind: PrincipalKind::Ai,
            name: name.into(),
        }
    }

    /// 系统主体。
    #[must_use]
    pub fn system() -> Self {
        Self {
            kind: PrincipalKind::System,
            name: "pinbridge".to_owned(),
        }
    }

    /// 类别。
    #[must_use]
    pub const fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// 展示名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 审计行：`ai:agent-7` 这种形式。
    #[must_use]
    pub fn audit_id(&self) -> String {
        format!("{}:{}", self.kind.tag(), self.name)
    }

    /// 归一化：同名不同类别视为不同主体。
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.kind == other.kind && self.name == other.name
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.audit_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_and_ai_with_same_name_are_different_principals() {
        let h = Principal::human("x");
        let a = Principal::ai("x");
        assert!(!h.same_as(&a));
        assert_eq!(h.audit_id(), "human:x");
        assert_eq!(a.to_string(), "ai:x");
        assert!(h.kind().is_external());
        assert!(!Principal::system().kind().is_external());
    }
}
