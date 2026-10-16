//! 强类型 ID 与序号。
//!
//! 刻意不用 `u64` 裸值：会话 ID、脚本 ID、线程 ID 都是 `u64`/`u32`，
//! 传参写反在编译期抓不出来，但会在"把 AI 脚本装到了错误的会话"这类
//! 事故里被抓出来。

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident, $inner:ty) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            /// 构造。
            #[must_use]
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            /// 取出底层整数。
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<$inner> for $name {
            fn from(value: $inner) -> Self {
                Self(value)
            }
        }
    };
}

typed_id!(
    /// 一次分析会话。会话是权限、脚本、轨迹的归属边界。
    SessionId, u64
);
typed_id!(
    /// 目标进程 PID（agent 侧为被 Pin 注入的进程自身）。
    ProcessId, u32
);
typed_id!(
    /// 目标进程内的线程 ID（OS 原生 tid）。
    ThreadId, u32
);
typed_id!(
    /// 一段注入脚本。
    ScriptId, u64
);
typed_id!(
    /// 一个 AI 客户端 / 自动化执行者。
    AgentId, u64
);
typed_id!(
    /// 事件在轨迹中的全局单调序号。重放、折叠、切片都依赖它。
    EventSeq, u64
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_types() {
        let s = SessionId::new(7);
        let p = ProcessId::new(7);
        assert_eq!(s.get(), u64::from(p.get()));
        assert_eq!(s.to_string(), "7");
        assert_eq!(SessionId::from(9u64), SessionId(9));
    }
}
