//! 脚本宿主：策略校验、登记与执行预算。
//!
//! 脚本进入目标进程之前要过三道关，本文件负责前两道与第三道的**记账**部分：
//!
//! ```text
//!   1. 策略（ScriptPolicy）  静态扫描源码：越界的 import 与逃逸用法直接拒绝
//!   2. 登记（ScriptHost）    id、名称、sha256、订阅集、生命周期状态
//!   3. 执行（M1）            内嵌 CPython；本文件只留 ScriptBudget 的记账
//! ```
//!
//! ## 策略层是什么、不是什么
//!
//! 它是**策略过滤器，不是安全边界**。静态扫描可以被构造性写法绕过
//! （`getattr(o, "__sub" + "classes__")` 这类）。真正的隔离在执行层：
//! M1 的 CPython 宿主会装 import hook、替换 `__builtins__`、砍掉
//! `open`/`socket` 等内建。这一点在 `docs/design/script-runtime.md` 里已经写明，
//! 这里再重复一次是因为**把它当成沙箱会得出错误的风险结论**。
//!
//! 那它为什么值得做：AI 写出 `import os` 的概率远高于写出绕过技巧的概率。
//! 在脚本**碰目标之前**用一句带行号与人话的错误把它拦下来，比等它跑起来
//! 再发现要便宜得多——而且拒绝的理由可以精确到"第 7 行的 os 不在允许清单里"。

use std::collections::BTreeMap;

use pinbridge_core::error::{Error, Result};

/// 单次脚本回调的默认时间预算（毫秒）。
///
/// 与 `pinbridge_core::SCRIPT_TIME_BUDGET_MS` 同值：Hub 侧用它做超时判断，
/// agent 侧用它记账。两处必须一致，因此这里直接引用 core 的常量。
pub const DEFAULT_CALL_BUDGET_MS: u64 = pinbridge_core::SCRIPT_TIME_BUDGET_MS;

/// 允许 import 的模块。
///
/// 刻意保持"够用但很小"：脚本要做的是**判断与记账**，不是通用编程。
/// 需要新模块时请先问一句"它给脚本增加了哪种能力"——凡是能碰文件、
/// 网络、进程、内存布局的，都不该进来。
pub const ALLOWED_MODULES: &[&str] = &[
    "math",
    "struct",
    "json",
    "re",
    "binascii",
    "collections",
    "itertools",
    "functools",
    "dataclasses",
    "typing",
    "enum",
    "array",
    "decimal",
    "string",
    "textwrap",
    "pb", // 本框架提供的接口（subscribe / log / mark / read / snapshot / defer）
];

/// 明令禁止的模块。
///
/// 与"不在允许清单里"的区别是**理由的清晰度**：命中这张表时，错误信息可以
/// 直接说"这会让你在目标进程里执行外部命令"，而不是干巴巴一句"不在白名单"。
/// AI 拿到前者会换方案，拿到后者容易在原地试别的写法。
pub const DENIED_MODULES: &[(&str, &str)] = &[
    ("os", "可以执行外部命令、读写任意文件"),
    ("sys", "可以改解释器状态、访问任意栈帧"),
    ("subprocess", "可以启动任意进程"),
    ("socket", "可以建立网络连接（把数据发出去）"),
    ("ctypes", "可以直接调用系统 API，绕过全部能力校验"),
    ("cffi", "同上：绕过能力校验直接调本地代码"),
    ("shutil", "可以删除/移动文件"),
    ("pathlib", "可以读写任意文件"),
    ("glob", "可以枚举文件系统"),
    ("tempfile", "可以写文件"),
    ("io", "可以打开任意文件句柄"),
    ("importlib", "可以绕过本策略动态导入"),
    ("builtins", "可以拿到全部内建函数"),
    ("multiprocessing", "可以创建进程"),
    ("threading", "可以在目标进程里另起执行流（会脱离预算控制）"),
    ("signal", "可以干扰目标进程的信号处理"),
    ("pickle", "反序列化可以执行任意代码"),
    ("marshal", "可以构造任意代码对象"),
    ("mmap", "可以直接映射内存"),
    ("urllib", "可以建立网络连接"),
    ("http", "可以建立网络连接"),
    ("ftplib", "可以建立网络连接"),
];

/// 源码里出现即拒绝的标识符（逃逸用法与危险内建）。
///
/// 每一项都带理由：这条信息会直接出现在给 AI 的错误里。
pub const DENIED_TOKENS: &[(&str, &str)] = &[
    ("__builtins__", "可以拿回被我们限制掉的全部内建函数"),
    (
        "__subclasses__",
        "可以顺着类继承树找到任意类型（经典逃逸手法）",
    ),
    ("__globals__", "可以读到其它函数的全局命名空间"),
    ("__code__", "可以改写代码对象"),
    ("__loader__", "可以拿到导入器"),
    ("__import__", "可以绕过 import 策略"),
    ("eval(", "可以执行任意表达式"),
    ("exec(", "可以执行任意代码"),
    ("compile(", "可以构造代码对象"),
    ("globals(", "可以读写全局命名空间"),
    ("locals(", "可以读写局部命名空间"),
    ("open(", "可以读写文件"),
    ("input(", "会阻塞目标线程"),
    (
        "breakpoint(",
        "会进入调试器（在目标进程里没有意义且会挂住）",
    ),
];

/// 一次策略违规。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyViolation {
    /// 行号（从 1 开始）。
    pub line: usize,
    /// 触发的 token（模块名或标识符）。
    pub token: String,
    /// 为什么不行。
    pub reason: String,
}

impl std::fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "第 {} 行用了 `{}`：{}",
            self.line, self.token, self.reason
        )
    }
}

/// 静态策略检查器。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptPolicy;

impl ScriptPolicy {
    /// 扫描源码。返回**第一处**违规（不是全部）。
    ///
    /// 为什么只返回第一处：脚本在被拒绝之后一定会改；一次给出十条
    /// 既没用（它得一条条改），也容易让人以为"改完这十条就安全了"。
    ///
    /// # Errors
    ///
    /// 违规时返回 [`PolicyViolation`] 的可读描述（进 `invalid_argument` 响应）。
    pub fn check(source: &str) -> std::result::Result<(), PolicyViolation> {
        for (index, raw_line) in source.lines().enumerate() {
            let line_no = index + 1;
            // 去掉注释与字符串字面量的干扰：脚本里写 `# import os 是被禁止的`
            // 不应该被判违规（那会把文档性注释也一起禁掉，反而逼人写得更糊涂）。
            let line = strip_comment(raw_line);

            for (token, reason) in DENIED_TOKENS {
                if line.contains(token) {
                    return Err(PolicyViolation {
                        line: line_no,
                        token: (*token).to_owned(),
                        reason: (*reason).to_owned(),
                    });
                }
            }

            for module in imported_modules(line) {
                if let Some((_, reason)) = DENIED_MODULES.iter().find(|(name, _)| *name == module) {
                    return Err(PolicyViolation {
                        line: line_no,
                        token: module,
                        reason: (*reason).to_owned(),
                    });
                }
                if !ALLOWED_MODULES.contains(&module.as_str()) {
                    return Err(PolicyViolation {
                        line: line_no,
                        token: module,
                        reason: format!("不在允许清单里（允许: {}）", ALLOWED_MODULES.join(", ")),
                    });
                }
            }
        }
        Ok(())
    }
}

/// 去掉行尾注释（不做完整的词法分析，够用即可——见模块文档的风险说明）。
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

/// 抽出一行里 `import` 的目标模块名。
///
/// 只处理两种形态：`import a.b as c, d` 与 `from a.b import x`。
/// 动态导入（`__import__`、`importlib`）已经在 token 层被拒。
fn imported_modules(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let mut out = Vec::new();

    if let Some(rest) = trimmed.strip_prefix("from ") {
        if let Some((module, _)) = rest.split_once(" import ") {
            // `from . import x` 这类相对导入也拒绝：它只会绕过模块名检查
            let name = module.trim();
            if name.is_empty() || name.starts_with('.') {
                out.push("<相对导入>".to_owned());
            } else {
                out.push(name.split('.').next().unwrap_or(name).to_owned());
            }
        }
        return out;
    }

    if let Some(rest) = trimmed.strip_prefix("import ") {
        for part in rest.split(',') {
            // `split_whitespace` 已经吃掉首尾空白，不需要先 trim
            let name = part.split_whitespace().next().unwrap_or("");
            if !name.is_empty() {
                out.push(name.split('.').next().unwrap_or(name).to_owned());
            }
        }
    }
    out
}

/// 脚本在宿主里的状态。
///
/// 与 `pinbridge-hub::script::ScriptState` 是**同一套生命周期语义**（有跨 crate
/// 的一致性测试），但多了一个 [`ScriptState::Registered`]：M0 只能做到"登记"，
/// 执行要等内嵌 CPython。把这个差别显式建模出来，比让 `Loaded` 同时表示
/// 两种事实要诚实——后者会让 Hub 以为脚本在跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptState {
    /// 已登记并通过策略校验，但**尚未执行**（运行时缺失）。
    Registered,
    /// 已加载并生效（M1 起由脚本运行时进入）。
    Loaded,
    /// 被同名新版本取代。
    Superseded,
    /// 已显式卸载。
    Unloaded,
    /// 超过执行预算被强制停止。
    TimedOut,
}

impl ScriptState {
    /// 协议里的短名（出现在 `AgentResponse::Script.state`）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Loaded => "loaded",
            Self::Superseded => "superseded",
            Self::Unloaded => "unloaded",
            Self::TimedOut => "timed_out",
        }
    }

    /// 还能不能再卸载（已卸载的再卸载应当报错，而不是假成功）。
    #[must_use]
    pub const fn can_unload(self) -> bool {
        !matches!(self, Self::Unloaded)
    }

    /// 是否仍会收到事件。
    ///
    /// 注意它与 [`ScriptState::receives_events`] **不是一回事**：
    /// `is_live` 回答的是"这段脚本还与我们有关吗"（生命周期问题，
    /// Superseded 也算，因为它可能还有在途回调），
    /// 而事件分发要问的是"现在该把新事件送给谁"——那只应该是 `Loaded`。
    ///
    /// 混用这两个判断会造成一个很难发现的 bug：被替换掉的旧脚本继续收到
    /// 新事件、继续打日志，而使用者以为它已经不生效了。
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Loaded | Self::Superseded)
    }

    /// 是否应该接收**新的**事件。
    #[must_use]
    pub const fn receives_events(self) -> bool {
        matches!(self, Self::Loaded)
    }
}

/// 一份已登记的脚本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// 脚本 ID（宿主内单调递增，从 1 开始）。
    pub id: u64,
    /// 名称（同名校验用）。
    pub name: String,
    /// 源码 sha256（小写 hex），与 Hub 侧算法一致（有跨 crate 测试）。
    pub source_sha256: String,
    /// 源码字节数。
    pub source_len: usize,
    /// 订阅的事件类型判别值（**存判别值而不是枚举**：这一层不依赖 trace，
    /// 也让"脚本订阅了什么"在协议里可以原样表达）。
    pub subscribe: Vec<u8>,
    /// 状态。
    pub state: ScriptState,
}

impl Script {
    /// 生命周期上是否仍与我们有关（含 Superseded）。
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.state.is_live()
    }

    /// 是否应该接收新事件（只有 `Loaded`）。
    #[must_use]
    pub const fn receives_events(&self) -> bool {
        self.state.receives_events()
    }
}

/// 目标进程内的脚本宿主。
///
/// 它保存的是**本进程内的视图**，不是权威登记表——权威在 Hub 侧
/// （`pinbridge-hub::script::ScriptRegistry`）。这个区分很重要：
/// 会话结束后 Hub 还留着历史，而目标进程一退出，这里的一切都没了。
#[derive(Debug, Default)]
pub struct ScriptHost {
    scripts: Vec<Script>,
    next_id: u64,
}

impl ScriptHost {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一段脚本：先过策略，再分配 id 与哈希。
    ///
    /// 同名旧脚本会被标成 `Superseded`——与 Hub 侧一致。
    ///
    /// # Errors
    ///
    /// 名/正文为空、策略违规时返回 [`Error::Script`]（消息里带行号与原因）。
    pub fn register(&mut self, name: &str, source: &str, subscribe: Vec<u8>) -> Result<Script> {
        if name.trim().is_empty() {
            return Err(Error::Script("脚本名不能为空（溯源需要稳定的名字）".into()));
        }
        if source.trim().is_empty() {
            return Err(Error::Script(format!("脚本 {name} 的正文为空")));
        }
        if let Err(violation) = ScriptPolicy::check(source) {
            return Err(Error::Script(format!(
                "脚本 {name} 违反运行策略：{violation}（策略见 docs/design/script-runtime.md）"
            )));
        }
        for existing in &mut self.scripts {
            if existing.name == name
                && matches!(
                    existing.state,
                    ScriptState::Loaded | ScriptState::Registered
                )
            {
                existing.state = ScriptState::Superseded;
            }
        }
        self.next_id += 1;
        let script = Script {
            id: self.next_id,
            name: name.to_owned(),
            source_sha256: crate::host::sha256_hex(source.as_bytes()),
            source_len: source.len(),
            subscribe,
            // M0：只能登记。执行由 M1 的运行时调用 `mark_loaded` 推进。
            state: ScriptState::Registered,
        };
        self.scripts.push(script.clone());
        Ok(script)
    }

    /// 标记为已加载并生效（M1 的运行时在 CPython 宿主就绪后调用）。
    ///
    /// # Errors
    ///
    /// 脚本不存在时返回错误。
    pub fn mark_loaded(&mut self, id: u64) -> Result<Script> {
        let script = self.find_mut(id)?;
        script.state = ScriptState::Loaded;
        Ok(script.clone())
    }

    /// 卸载。
    ///
    /// # Errors
    ///
    /// 不存在或已卸载时返回错误（不接受"假成功"）。
    pub fn unregister(&mut self, id: u64) -> Result<Script> {
        let script = self.find_mut(id)?;
        if !script.state.can_unload() {
            return Err(Error::Script(format!("脚本 {id} 已经卸载过了")));
        }
        script.state = ScriptState::Unloaded;
        Ok(script.clone())
    }

    /// 标记超时。
    ///
    /// # Errors
    ///
    /// 脚本不存在时返回错误。
    pub fn mark_timed_out(&mut self, id: u64) -> Result<Script> {
        let script = self.find_mut(id)?;
        script.state = ScriptState::TimedOut;
        Ok(script.clone())
    }

    /// 按 ID 查询。
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Script> {
        self.scripts.iter().find(|s| s.id == id)
    }

    /// 全部脚本（含历史版本）。
    #[must_use]
    pub fn all(&self) -> &[Script] {
        &self.scripts
    }

    /// 按事件类型判别值找订阅者（事件分发用）。
    #[must_use]
    pub fn subscribers(&self, kind: u8) -> Vec<&Script> {
        self.scripts
            .iter()
            .filter(|s| s.receives_events() && s.subscribe.contains(&kind))
            .collect()
    }

    fn find_mut(&mut self, id: u64) -> Result<&mut Script> {
        self.scripts
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::Script(format!("脚本 {id} 不存在")))
    }
}

/// 单次脚本回调的预算记账。
///
/// 记账与**强制**是两件事：这里只回答"超了没有"。真正的强制需要运行时配合
/// （M1：看门狗线程 + 让解释器在下一个检查点退出），因为 Python 里的死循环
/// 不会自己让出控制权。
#[derive(Debug, Clone)]
pub struct ScriptBudget {
    per_call_ms: u64,
    /// 每个脚本的累计耗时与超时次数。
    stats: BTreeMap<u64, ScriptTiming>,
}

/// 单个脚本的时间统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScriptTiming {
    /// 回调被调用次数。
    pub calls: u64,
    /// 单次最长耗时（毫秒）。
    pub max_ms: u64,
    /// 累计耗时（毫秒）。
    pub total_ms: u64,
    /// 超预算次数。
    pub overruns: u64,
}

impl ScriptBudget {
    /// 新建，指定单次回调上限（毫秒）。0 表示不限制。
    #[must_use]
    pub const fn new(per_call_ms: u64) -> Self {
        Self {
            per_call_ms,
            stats: BTreeMap::new(),
        }
    }

    /// 单次上限。
    #[must_use]
    pub const fn per_call_ms(&self) -> u64 {
        self.per_call_ms
    }

    /// 记一次回调耗时。返回是否超预算。
    ///
    /// 超预算**一定会被记账**（`overruns`）：静默放行会让"这段脚本为什么让
    /// 目标卡住"变成无法回答的问题。
    pub fn record(&mut self, script: u64, elapsed_ms: u64) -> bool {
        let over = self.per_call_ms != 0 && elapsed_ms > self.per_call_ms;
        let entry = self.stats.entry(script).or_default();
        entry.calls += 1;
        entry.total_ms = entry.total_ms.saturating_add(elapsed_ms);
        entry.max_ms = entry.max_ms.max(elapsed_ms);
        if over {
            entry.overruns += 1;
        }
        over
    }

    /// 某个脚本的统计。
    #[must_use]
    pub fn timing(&self, script: u64) -> ScriptTiming {
        self.stats.get(&script).copied().unwrap_or_default()
    }

    /// 有脚本超预算吗（用于在 summary 里如实上报）。
    #[must_use]
    pub fn any_overrun(&self) -> bool {
        self.stats.values().any(|t| t.overruns > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 策略 ────────────────────────────────────────────────────────────

    #[test]
    fn allowed_scripts_pass_the_policy() {
        let source = r#"
import pb
import math, struct
from collections import OrderedDict

def on_syscall(ev):
    # 注释里写 import os 不应该被判违规
    total = math.floor(ev.args[0] / 8)
    pb.log("hit %d" % total)
"#;
        assert!(ScriptPolicy::check(source).is_ok());
    }

    #[test]
    fn denied_imports_are_rejected_with_line_and_reason() {
        let source = "import pb\nimport os\n";
        let violation = ScriptPolicy::check(source).unwrap_err();
        assert_eq!(violation.line, 2, "行号必须准确：AI 要据此改代码");
        assert_eq!(violation.token, "os");
        assert!(
            violation.reason.contains("外部命令"),
            "拒绝理由要说清「这会给你什么能力」: {}",
            violation.reason
        );
        let text = violation.to_string();
        assert!(text.contains("第 2 行"), "实际: {text}");
    }

    #[test]
    fn unknown_module_is_rejected_and_the_allowlist_is_shown() {
        let violation = ScriptPolicy::check("import flask\n").unwrap_err();
        assert_eq!(violation.token, "flask");
        assert!(
            violation.reason.contains("math"),
            "不在白名单时要给出可用清单: {}",
            violation.reason
        );
    }

    #[test]
    fn from_import_and_relative_import_are_covered() {
        assert_eq!(
            ScriptPolicy::check("from subprocess import Popen\n")
                .unwrap_err()
                .token,
            "subprocess"
        );
        // 相对导入会绕过模块名检查，必须拒绝
        assert_eq!(
            ScriptPolicy::check("from . import helper\n")
                .unwrap_err()
                .token,
            "<相对导入>"
        );
        // 点号路径只看顶层模块名
        assert!(ScriptPolicy::check("import collections.abc\n").is_ok());
        assert_eq!(
            ScriptPolicy::check("import ctypes.wintypes\n")
                .unwrap_err()
                .token,
            "ctypes"
        );
    }

    #[test]
    fn escape_hatches_are_rejected() {
        for (source, token) in [
            ("x = __builtins__\n", "__builtins__"),
            ("c = object.__subclasses__()\n", "__subclasses__"),
            ("eval(\"1+1\")\n", "eval("),
            ("exec(code)\n", "exec("),
            ("f = open('/etc/passwd')\n", "open("),
            ("m = __import__('os')\n", "__import__"),
        ] {
            let violation = ScriptPolicy::check(source).unwrap_err();
            assert_eq!(violation.token, token, "源码: {source}");
        }
    }

    #[test]
    fn denial_is_not_a_sandbox_and_we_say_so() {
        // 构造性绕过是可能的——策略层的定位是"防意外"，不是安全边界。
        // 这条测试把这件事写成断言，免得将来有人把它当沙箱用。
        let sneaky = format!("m = getattr({}, '__sub' + 'classes__')\n", "object");
        assert!(
            ScriptPolicy::check(&sneaky).is_ok(),
            "静态扫描**拦不住**构造性绕过；真正的隔离在 M1 的解释器层（见模块文档）"
        );
    }

    // ── 登记 ────────────────────────────────────────────────────────────

    #[test]
    fn registration_assigns_ids_hashes_and_states() {
        let mut host = ScriptHost::new();
        let first = host
            .register("hook.py", "import pb\npb.log('a')\n", vec![3, 6])
            .expect("应当登记成功");
        assert_eq!(first.id, 1);
        assert_eq!(first.state, ScriptState::Registered);
        assert_eq!(first.state.name(), "registered");
        assert_eq!(first.source_len, "import pb\npb.log('a')\n".len());
        // 与 Hub 侧常量一致（DEMO_SOURCE 的 sha256 见 hub 的测试）
        assert_eq!(
            first.source_sha256,
            crate::host::sha256_hex(b"import pb\npb.log('a')\n")
        );

        let second = host
            .register("hook.py", "import pb\npb.log('b')\n", vec![6])
            .expect("同名应当允许");
        assert_eq!(second.id, 2);
        // 同名旧版本被取代（与 Hub 侧语义一致）
        assert_eq!(host.get(1).unwrap().state, ScriptState::Superseded);
        assert_eq!(host.get(2).unwrap().state, ScriptState::Registered);

        // M0 的关键差别：登记 ≠ 运行。两个脚本都还没被运行时加载，
        // 因此**都不应该被算作订阅者**——否则事件分发会把事件送给一段
        // 根本没在执行的代码，而调用方看不出任何异常。
        assert!(
            host.subscribers(6).is_empty(),
            "只是登记过的脚本不该收到事件"
        );

        // 运行时（M1）推进状态之后才成为订阅者
        host.mark_loaded(2).unwrap();
        let live = host.subscribers(6);
        assert_eq!(live.len(), 1, "只有已加载的第二版订阅 6");
        assert_eq!(live[0].id, 2);
        // 这里体现两个判断的区别（两者混用会造成很难发现的 bug）：
        //   * is_live：生命周期上"还与我们有关"——Superseded 也算（可能有在途回调）；
        //   * receives_events：现在该把**新**事件送给谁——只有 Loaded。
        // 第一版已被第二版取代，因此它仍然 is_live，但**不再收新事件**：
        // 否则被替换掉的旧脚本会继续按老逻辑打日志，而使用者以为它已经失效。
        assert!(host.get(1).unwrap().is_live(), "Superseded 仍算 live");
        assert!(!host.get(1).unwrap().receives_events());
        assert!(
            host.subscribers(3).is_empty(),
            "只有被取代的第一版订阅过 3，它不该再收新事件"
        );
    }

    #[test]
    fn registration_rejects_empty_and_policy_violating_sources() {
        let mut host = ScriptHost::new();
        assert!(host.register("", "import pb\n", vec![]).is_err());
        assert!(host.register("a.py", "   ", vec![]).is_err());
        let error = host.register("a.py", "import os\n", vec![]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("第 1 行"), "实际: {text}");
        assert!(text.contains("os"), "实际: {text}");
        assert!(host.all().is_empty(), "被拒的脚本不应该留在宿主里");
    }

    #[test]
    fn unregister_is_not_idempotent_and_timeout_is_recorded() {
        let mut host = ScriptHost::new();
        let script = host.register("a.py", "import pb\n", vec![]).unwrap();
        assert_eq!(
            host.unregister(script.id).unwrap().state,
            ScriptState::Unloaded
        );
        // 再卸载必须报错，而不是假成功
        assert!(host.unregister(script.id).is_err());
        assert!(host.unregister(999).is_err());

        let other = host.register("b.py", "import pb\n", vec![]).unwrap();
        assert_eq!(
            host.mark_timed_out(other.id).unwrap().state,
            ScriptState::TimedOut
        );
        assert!(!ScriptState::TimedOut.is_live());
    }

    #[test]
    fn runtime_can_promote_a_registered_script() {
        // M1 的运行时把"已登记"推进到"已加载"；这条迁移现在就有测试。
        let mut host = ScriptHost::new();
        let script = host.register("a.py", "import pb\n", vec![]).unwrap();
        assert_eq!(script.state, ScriptState::Registered);
        assert!(host.subscribers(0).is_empty(), "未加载的脚本不该收到事件");
        let loaded = host.mark_loaded(script.id).unwrap();
        assert_eq!(loaded.state, ScriptState::Loaded);
        assert!(host.mark_loaded(999).is_err());
    }

    // ── 预算 ────────────────────────────────────────────────────────────

    #[test]
    fn budget_records_overruns_instead_of_swallowing_them() {
        let mut budget = ScriptBudget::new(5);
        assert!(!budget.record(1, 3));
        assert!(!budget.record(1, 5), "正好等于上限不算超");
        assert!(budget.record(1, 9));
        let timing = budget.timing(1);
        assert_eq!(timing.calls, 3);
        assert_eq!(timing.max_ms, 9);
        assert_eq!(timing.total_ms, 17);
        assert_eq!(timing.overruns, 1);
        assert!(budget.any_overrun());
        // 未出现的脚本：全零，而不是 panic
        assert_eq!(budget.timing(42), ScriptTiming::default());
    }

    #[test]
    fn zero_budget_means_unlimited_but_still_accounted() {
        let mut budget = ScriptBudget::new(0);
        assert!(!budget.record(1, 10_000));
        assert!(!budget.any_overrun());
        assert_eq!(budget.timing(1).calls, 1);
    }
}
