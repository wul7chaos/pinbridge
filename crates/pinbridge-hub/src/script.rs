//! 脚本注册表。
//!
//! 脚本是 PinBridge 的主要工作单元：AI 写 Python、注入目标进程、观察结果、
//! 再改一版。这个注册表要回答的问题是：
//!
//! * **现在跑的是哪一版？**（同名脚本被替换时旧版必须显式进入 `Superseded`，
//!   而不是被悄悄覆盖——"我明明改了脚本为什么行为没变"是最浪费时间的一类 bug）
//! * **这一版的内容是什么？**（`source_sha256` 是跨语言、跨机器的稳定标识）
//! * **它订阅了哪些事件？**（订阅集决定它会不会被唤醒，也就决定了开销）

use pinbridge_core::error::{Error, Result};
use pinbridge_core::id::ScriptId;
use pinbridge_core::time::now_unix_millis;
use pinbridge_trace::event::EventKind;
use sha2::{Digest, Sha256};

/// 脚本在生命周期中所处的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptState {
    /// 已加载并生效。
    Loaded,
    /// 已被同名的新版本取代。**不会自动卸载**：它可能还有在途回调，
    /// 强行清掉会造成目标进程内的中间状态不一致。
    Superseded,
    /// 已被显式卸载。
    Unloaded,
    /// 超过执行预算被强制停止（见 `README` 的脚本运行模型）。
    TimedOut,
}

impl ScriptState {
    /// 是否仍会收到事件。
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Loaded | Self::Superseded)
    }

    /// 是否还能被卸载（已卸载的脚本再卸载应当报错而不是静默成功）。
    #[must_use]
    pub const fn can_unload(self) -> bool {
        !matches!(self, Self::Unloaded)
    }
}

/// 一段已注册的脚本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// 脚本 ID。
    pub id: ScriptId,
    /// 名称（同名校验用）。
    pub name: String,
    /// 源码 sha256（hex，小写）。
    pub source_sha256: String,
    /// 源码字节数。
    pub source_len: usize,
    /// 加载时间（Unix 毫秒）。
    pub loaded_at_ms: u64,
    /// 订阅的事件类型。
    pub subscribe: Vec<EventKind>,
    /// 状态。
    pub state: ScriptState,
}

impl Script {
    /// 是否订阅某个事件类型。
    #[must_use]
    pub fn subscribes(&self, kind: EventKind) -> bool {
        self.subscribe.contains(&kind)
    }
}

/// 脚本注册表。按会话持有（脚本是会话作用域的，不跨会话共享）。
#[derive(Debug, Default)]
pub struct ScriptRegistry {
    scripts: Vec<Script>,
    next_id: u64,
}

impl ScriptRegistry {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一段脚本。同名旧脚本会被标记为 `Superseded`。
    pub fn load(&mut self, name: &str, source: &str, subscribe: Vec<EventKind>) -> Result<Script> {
        if name.trim().is_empty() {
            return Err(Error::Script("脚本名不能为空（溯源需要稳定的名字）".into()));
        }
        if source.trim().is_empty() {
            return Err(Error::Script(format!("脚本 {name} 的正文为空")));
        }
        for existing in &mut self.scripts {
            if existing.name == name && existing.state == ScriptState::Loaded {
                existing.state = ScriptState::Superseded;
            }
        }
        self.next_id += 1;
        let script = Script {
            id: ScriptId::new(self.next_id),
            name: name.to_owned(),
            source_sha256: sha256_hex(source.as_bytes()),
            source_len: source.len(),
            loaded_at_ms: now_unix_millis(),
            subscribe,
            state: ScriptState::Loaded,
        };
        self.scripts.push(script.clone());
        Ok(script)
    }

    /// 卸载。已卸载的脚本再次卸载返回错误（不接受"假成功"）。
    pub fn unload(&mut self, id: ScriptId) -> Result<Script> {
        let Some(script) = self.scripts.iter_mut().find(|s| s.id == id) else {
            return Err(Error::Script(format!("脚本 {id} 不存在")));
        };
        if !script.state.can_unload() {
            return Err(Error::Script(format!("脚本 {id} 已经卸载过了")));
        }
        script.state = ScriptState::Unloaded;
        Ok(script.clone())
    }

    /// 标记超时。
    pub fn mark_timed_out(&mut self, id: ScriptId) -> Result<()> {
        let Some(script) = self.scripts.iter_mut().find(|s| s.id == id) else {
            return Err(Error::Script(format!("脚本 {id} 不存在")));
        };
        script.state = ScriptState::TimedOut;
        Ok(())
    }

    /// 按 ID 查询。
    #[must_use]
    pub fn get(&self, id: ScriptId) -> Option<&Script> {
        self.scripts.iter().find(|s| s.id == id)
    }

    /// 全部脚本（含历史版本）。
    #[must_use]
    pub fn all(&self) -> &[Script] {
        &self.scripts
    }

    /// 当前生效的脚本。
    pub fn live(&self) -> impl Iterator<Item = &Script> {
        self.scripts
            .iter()
            .filter(|s| s.state == ScriptState::Loaded)
    }

    /// 注册过的脚本总数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.scripts.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }
}

/// 计算 sha256 的 hex 表示（小写）。
///
/// 这一项**必须与 Python 侧 `hashlib.sha256(...).hexdigest()` 完全一致**，
/// 否则"AI 说它加载了某一版脚本"就无法与轨迹里的 AUDIT 记录对上。
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO_SOURCE: &str = "# pinbridge demo hook\nprint('hello')\n";
    /// 由 `python -c "import hashlib;print(hashlib.sha256(b'# pinbridge demo hook\nprint(\'hello\')\n').hexdigest())"`
    /// 得到，与 `python/tests/test_hub_parity.py` 中的常量相同。
    const DEMO_SHA256: &str = "a98c7127446444ec31a9890062ee7f669d02b955bf07ea513f23f5f3db81671e";

    #[test]
    fn sha256_matches_known_vector_and_python() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(DEMO_SOURCE.as_bytes()), DEMO_SHA256);
    }

    #[test]
    fn reloading_same_name_supersedes_previous_version() {
        let mut registry = ScriptRegistry::new();
        let first = registry
            .load("hook.py", DEMO_SOURCE, vec![EventKind::BreakpointHit])
            .unwrap();
        let second = registry
            .load(
                "hook.py",
                "# v2\nprint('v2')\n",
                vec![EventKind::BreakpointHit],
            )
            .unwrap();
        assert_eq!(registry.len(), 2, "历史版本必须保留");
        assert_eq!(
            registry.get(first.id).unwrap().state,
            ScriptState::Superseded
        );
        assert_eq!(registry.get(second.id).unwrap().state, ScriptState::Loaded);
        assert_eq!(registry.live().count(), 1);
        assert_ne!(first.source_sha256, second.source_sha256);
    }

    #[test]
    fn empty_name_or_source_is_rejected() {
        let mut registry = ScriptRegistry::new();
        assert!(registry.load("  ", DEMO_SOURCE, vec![]).is_err());
        assert!(registry.load("ok.py", "   \n", vec![]).is_err());
    }

    #[test]
    fn double_unload_is_an_error() {
        let mut registry = ScriptRegistry::new();
        let script = registry.load("hook.py", DEMO_SOURCE, vec![]).unwrap();
        registry.unload(script.id).unwrap();
        assert_eq!(
            registry.get(script.id).unwrap().state,
            ScriptState::Unloaded
        );
        let err = registry.unload(script.id).unwrap_err();
        assert!(err.to_string().contains("已经卸载"));
        assert!(registry.unload(ScriptId::new(999)).is_err());
    }

    #[test]
    fn subscription_is_checkable() {
        let mut registry = ScriptRegistry::new();
        let script = registry
            .load(
                "hook.py",
                DEMO_SOURCE,
                vec![EventKind::SyscallEnter, EventKind::MemWrite],
            )
            .unwrap();
        let stored = registry.get(script.id).unwrap();
        assert!(stored.subscribes(EventKind::SyscallEnter));
        assert!(stored.subscribes(EventKind::MemWrite));
        assert!(!stored.subscribes(EventKind::MemRead));
        assert!(ScriptState::Superseded.is_live());
        assert!(!ScriptState::TimedOut.is_live());
    }

    #[test]
    fn timeout_marks_script_dead() {
        let mut registry = ScriptRegistry::new();
        let script = registry.load("hook.py", DEMO_SOURCE, vec![]).unwrap();
        registry.mark_timed_out(script.id).unwrap();
        assert_eq!(
            registry.get(script.id).unwrap().state,
            ScriptState::TimedOut
        );
        assert_eq!(registry.live().count(), 0);
    }
}
