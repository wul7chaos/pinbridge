//! MCP 工具描述表。
//!
//! ## 为什么这里只有"薄"工具
//!
//! PinBridge 的立场是：**不给 AI 提供它做不到的事情的替代品**。
//! 传统"调试器 + MCP"方案把 MCP 工具做成功能菜单（读内存、读寄存器、下断点……），
//! AI 只能在这些固定函数之间排列组合，遇到脱壳这种"先判断 OEP → 再决定 Dump 时机 →
//! 再修内存属性"的组合拳就只能靠 token 硬拼。
//!
//! 因此 MCP 层刻意保持薄：它只负责**开一个门**（[`TOOLS`] 里的会话/脚本/事件类工具），
//! 真正的分析逻辑由 AI 写 Python 脚本注入目标进程执行（`pb_script_load`）。
//! 脚本能做 MCP 工具做不到的事：任意循环、条件分支、试错与回退。
//!
//! ## 约定
//!
//! * 工具名统一 `pb_` 前缀，避免与别的 MCP server 撞名；
//! * 每个工具的 `description` 必须包含"什么时候用它"和"它不是什么"；
//! * `requires` 声明所需能力，`pb_` 服务端在转发前先校验，失败直接返回错误，
//!   不把请求丢给 agent（避免在目标进程里制造错误现场）。

use pinbridge_core::capability::Capability;

/// 一个 MCP 工具的描述。
#[derive(Debug, Clone, Copy)]
pub struct ToolDescriptor {
    /// 工具名（`pb_` 前缀）。
    pub name: &'static str,
    /// 给 AI 看的说明。写清使用时机与边界。
    pub description: &'static str,
    /// JSON Schema（字符串形态，因为 `serde_json::json!` 不是 const）。
    pub input_schema_json: &'static str,
    /// 调用所需能力。
    pub requires: &'static [Capability],
    /// 是否会在目标进程内执行代码（这类调用必须在审计里留痕）。
    pub executes_in_target: bool,
}

/// 全部工具。
pub const TOOLS: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: "pb_session_list",
        description: "列出当前所有分析会话（含控制权持有者与状态）。开始任何工作前先调用它确定 session id。",
        input_schema_json: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
        requires: &[],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_session_create",
        description: "启动目标程序并建立会话，返回 session id。仅用于启动新目标；附加到已运行进程请用 pb_session_attach。",
        input_schema_json: r#"{"type":"object","properties":{"image_path":{"type":"string"},"arch":{"type":"string","enum":["x86_64","i386"]},"args":{"type":"array","items":{"type":"string"}}},"required":["image_path"],"additionalProperties":false}"#,
        requires: &[Capability::ProcessControl],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_session_attach",
        description: "附加到已运行的进程（按 pid）。附加后目标默认处于暂停状态。",
        input_schema_json: r#"{"type":"object","properties":{"pid":{"type":"integer"},"arch":{"type":"string"}},"required":["pid"],"additionalProperties":false}"#,
        requires: &[Capability::ProcessControl],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_control",
        description: "下发控制命令：continue / pause / step / run_to。返回的是『已受理』而非『已完成』，中断类命令需随后用 pb_events_query 观察是否落地。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"command":{"type":"object","properties":{"cmd":{"type":"string","enum":["continue","pause","step","run_to"]},"count":{"type":"integer"},"address":{"type":"integer"}},"required":["cmd"]}},"required":["session","command"],"additionalProperties":false}"#,
        requires: &[Capability::SingleStep],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_memory_read",
        description: "读取目标内存切片（hex）。用于验证假设，不适合批量抽取——批量请用脚本在目标进程内完成再回传摘要。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"address":{"type":"integer"},"length":{"type":"integer","maximum":65536}},"required":["session","address","length"],"additionalProperties":false}"#,
        requires: &[Capability::MemRead],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_breakpoint_set",
        description: "在指定地址下断点。地址必须是目标进程的虚拟地址（模块基址 + RVA 需自行换算，可用 pb_modules 获取基址）。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"address":{"type":"integer"},"label":{"type":"string"}},"required":["session","address"],"additionalProperties":false}"#,
        requires: &[Capability::Breakpoint],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_script_load",
        description: "向目标进程注入一段 Python 脚本（内嵌 CPython，回调在目标线程上运行）。这是本框架的主要工作方式：脚本负责判断与决策，MCP 工具只负责观察与放行。脚本有执行时间预算，超时会被强制卸载。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"name":{"type":"string"},"source":{"type":"string"},"subscribe":{"type":"array","items":{"type":"string"}}},"required":["session","name","source"],"additionalProperties":false}"#,
        requires: &[Capability::ScriptLoad],
        executes_in_target: true,
    },
    ToolDescriptor {
        name: "pb_events_query",
        description: "按序号拉取事件批（默认折叠连续重复事件并给出 repeats 计数）。这是观察目标行为的主通道；dropped 字段非零说明事件被背压丢弃，结论需要打折。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"from":{"type":"integer"},"limit":{"type":"integer","maximum":4096},"fold_repeats":{"type":"boolean"}},"required":["session","from"],"additionalProperties":false}"#,
        requires: &[Capability::MemRead],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_trace_record",
        description: "开始/停止轨迹录制并返回 .pbt 路径。轨迹用于离线深度分析（污点传播、反向切片），不要在对抗强度高的目标上全程开启。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"action":{"type":"string","enum":["start","stop"]},"path":{"type":"string"},"profile":{"type":"string","enum":["none","repeat","repeat+delta"]}},"required":["session","action"],"additionalProperties":false}"#,
        requires: &[Capability::TraceRecord],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_provenance",
        description: "读取本次会话的审计链：谁（人或 AI）在何时加载了哪一版脚本、下发了什么命令、结果如何。当你需要解释『为什么内存现在是这个状态』时用它。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"}},"required":["session"],"additionalProperties":false}"#,
        requires: &[],
        executes_in_target: false,
    },
    ToolDescriptor {
        name: "pb_capability_grant",
        description: "授予或收窄某个主体的能力集。只读会话默认没有该工具；这是人类保留的最终阀门。",
        input_schema_json: r#"{"type":"object","properties":{"session":{"type":"integer"},"target_principal":{"type":"string"},"capabilities":{"type":"array","items":{"type":"string"}}},"required":["session","target_principal","capabilities"],"additionalProperties":false}"#,
        requires: &[Capability::CapabilityGrant],
        executes_in_target: false,
    },
];

/// 按名字查工具。
#[must_use]
pub fn find(name: &str) -> Option<&'static ToolDescriptor> {
    TOOLS.iter().find(|t| t.name == name)
}

/// 解析某个工具的 JSON Schema。整个仓库只在真正需要发 `tools/list` 时解析一次。
pub fn schema_of(tool: &ToolDescriptor) -> pinbridge_core::error::Result<serde_json::Value> {
    serde_json::from_str(tool.input_schema_json).map_err(|e| {
        pinbridge_core::error::Error::Protocol(format!("工具 {} 的 schema 非法: {e}", tool.name))
    })
}

/// 供 `tools/list` 直接使用的 JSON 数组。
pub fn tools_list_json() -> pinbridge_core::error::Result<serde_json::Value> {
    let mut items = Vec::with_capacity(TOOLS.len());
    for tool in TOOLS {
        items.push(serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "inputSchema": schema_of(tool)?,
        }));
    }
    Ok(serde_json::Value::Array(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_prefixed() {
        let mut names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "工具名必须唯一");
        for name in names {
            assert!(name.starts_with("pb_"), "{name} 缺少 pb_ 前缀");
        }
    }

    #[test]
    fn every_tool_has_a_valid_schema() {
        for tool in TOOLS {
            let schema = schema_of(tool).unwrap_or_else(|e| panic!("{}: {e}", tool.name));
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{} 的 schema 顶层必须是 object",
                tool.name
            );
        }
    }

    #[test]
    fn only_script_load_executes_in_target() {
        let in_target: Vec<&str> = TOOLS
            .iter()
            .filter(|t| t.executes_in_target)
            .map(|t| t.name)
            .collect();
        assert_eq!(in_target, vec!["pb_script_load"]);
    }

    #[test]
    fn lookup_and_list_agree() {
        let list = tools_list_json().unwrap();
        assert_eq!(list.as_array().unwrap().len(), TOOLS.len());
        assert!(find("pb_session_list").is_some());
        assert!(find("pb_nope").is_none());
    }

    #[test]
    fn tool_requiring_grant_is_the_only_grant_tool() {
        let grant_tools: Vec<&str> = TOOLS
            .iter()
            .filter(|t| t.requires.contains(&Capability::CapabilityGrant))
            .map(|t| t.name)
            .collect();
        assert_eq!(grant_tools, vec!["pb_capability_grant"]);
    }
}
