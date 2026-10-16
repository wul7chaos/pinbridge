//! MCP 服务实现（JSON-RPC 2.0 over stdio）。

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::error::{Error, Result};
use pinbridge_core::id::SessionId;
use pinbridge_core::principal::Principal;
use pinbridge_core::session::TargetSpec;
use pinbridge_hub::HubService;
use pinbridge_proto::frames::ControlCommand;
use pinbridge_proto::mcp::{find, schema_of, TOOLS};
use serde_json::{json, Value};

/// JSON-RPC / MCP 协议级错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    /// JSON-RPC 错误码。
    pub code: i64,
    /// 描述。
    pub message: String,
}

impl ProtocolError {
    /// 请求不是合法 JSON。
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self {
            code: -32700,
            message: message.into(),
        }
    }

    /// 方法不存在。
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("未知方法: {method}"),
        }
    }

    /// 参数非法。
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    /// 内部错误（包含工具执行失败）。
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

/// MCP 服务状态。
#[derive(Debug)]
pub struct McpServer {
    hub: HubService,
    /// 调用方主体。`initialize` 时由 `clientInfo.name` 推出。
    actor: Principal,
    /// 会话 → epoch 缓存（理由见 crate 文档）。
    epochs: BTreeMap<u64, u64>,
    /// 能力集（由 Hub 授权结果决定，`tools/call` 前先自查一遍）。
    capabilities: CapabilitySet,
    initialized: bool,
}

impl McpServer {
    /// 新建。
    #[must_use]
    pub fn new(hub: HubService) -> Self {
        let mut server = Self {
            hub,
            actor: Principal::ai("mcp-client"),
            epochs: BTreeMap::new(),
            capabilities: CapabilitySet::empty(),
            initialized: false,
        };
        server.capabilities = server.hub.capabilities_of(&server.actor.clone());
        server
    }

    /// 调用方主体。
    #[must_use]
    pub const fn actor(&self) -> &Principal {
        &self.actor
    }

    /// 是否已完成 `initialize`。
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// 处理一条 JSON-RPC 消息。返回 `None` 表示这是通知（无需响应）。
    pub fn handle(&mut self, message: &str) -> Option<Value> {
        let parsed: Value = match serde_json::from_str(message) {
            Ok(value) => value,
            Err(e) => {
                return Some(error_response(
                    Value::Null,
                    &ProtocolError::parse(e.to_string()),
                ));
            }
        };
        let id = parsed.get("id").cloned();
        let Some(method) = parsed.get("method").and_then(Value::as_str) else {
            return Some(error_response(
                id.unwrap_or(Value::Null),
                &ProtocolError::invalid_params("缺少 method 字段"),
            ));
        };
        let params = parsed.get("params").cloned().unwrap_or(Value::Null);

        let Some(id) = id else {
            // 通知：按 JSON-RPC 2.0 不返回任何内容，但仍然要执行（例如
            // `notifications/initialized` 之后的副作用）。
            let _ = self.dispatch(method, &params);
            return None;
        };
        let outcome = self.dispatch(method, &params);
        Some(match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => error_response(id, &error),
        })
    }

    fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, ProtocolError> {
        match method {
            "initialize" => self.initialize(params),
            "notifications/initialized" => Ok(json!({})),
            "ping" => Ok(json!({})),
            "tools/list" => self.tools_list(),
            "tools/call" => self.tools_call(params),
            "shutdown" => Ok(json!({})),
            other => Err(ProtocolError::method_not_found(other)),
        }
    }

    fn initialize(&mut self, params: &Value) -> std::result::Result<Value, ProtocolError> {
        let client_name = params
            .get("clientInfo")
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("mcp-client");
        self.actor = Principal::ai(client_name);
        self.capabilities = self.hub.capabilities_of(&self.actor.clone());
        self.initialized = true;
        Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "pinbridge-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "PinBridge 是插装式动态分析框架。会话附在后先用 pb_events_query 观察，\
        用 pb_script_load 注入分析脚本。所有写操作都受能力集与控制权 epoch 约束，\
        失败时请读返回的 hint 字段。"
        }))
    }

    fn tools_list(&mut self) -> std::result::Result<Value, ProtocolError> {
        let mut items = Vec::with_capacity(TOOLS.len());
        for tool in TOOLS {
            // 只暴露当前能力允许的工具：让 AI 看不到它做不到的事，
            // 比让它反复尝试再失败更省 token，也更少产生"我是不是该换个办法"的幻觉。
            if tool
                .requires
                .iter()
                .all(|capability| self.capabilities.contains(*capability))
            {
                items.push(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": schema_of(tool).map_err(|e| ProtocolError::internal(e.to_string()))?,
                }));
            }
        }
        Ok(json!({"tools": items}))
    }

    fn tools_call(&mut self, params: &Value) -> std::result::Result<Value, ProtocolError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError::invalid_params("tools/call 缺少 name"))?;
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let Some(tool) = find(name) else {
            return Err(ProtocolError::invalid_params(format!("未知工具: {name}")));
        };
        // 能力自查（Hub 侧还会再校验一次）
        for capability in tool.requires {
            if !self.capabilities.contains(*capability) {
                return Ok(tool_error(format!(
                    "能力不足：{name} 需要 {}，当前持有 [{}]。请让人类通过 pb_capability_grant 授权。",
                    capability.name(),
                    self.capabilities
                        .iter()
                        .map(Capability::name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        match self.invoke(name, &arguments) {
            Ok(text) => Ok(json!({"content": [{"type": "text", "text": text}], "isError": false})),
            Err(message) => Ok(tool_error(message)),
        }
    }

    /// 工具实现。返回值直接作为 `tool_result` 的文本内容。
    fn invoke(&mut self, name: &str, args: &Value) -> std::result::Result<String, String> {
        match name {
            "pb_session_list" => {
                let sessions = self.hub.sessions();
                to_text(&sessions)
            }
            "pb_session_create" => {
                let image = arg_str(args, "image_path")?;
                let arch = args.get("arch").and_then(Value::as_str).unwrap_or("x86_64");
                let id = self
                    .hub
                    .create_session(&self.actor.clone(), TargetSpec::launch(image.clone(), arch))
                    .map_err(describe)?;
                self.epochs
                    .insert(id.get(), self.hub.epoch_of(id).map_err(describe)?);
                Ok(format!(
                    "已创建会话 {id}（目标 {image}）。附加请调用 pb_session_attach。"
                ))
            }
            "pb_session_attach" => {
                let pid = arg_u64(args, "pid")? as u32;
                let session = self.session_arg(args)?;
                let command_line = args
                    .get("command_line")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.hub
                    .attach(&self.actor.clone(), session, pid, command_line)
                    .map_err(describe)?;
                let epoch = self.hub.epoch_of(session).map_err(describe)?;
                self.epochs.insert(session.get(), epoch);
                Ok(format!(
                    "已附加到 pid={pid}（会话 {session}，epoch={epoch}）"
                ))
            }
            "pb_control" => {
                let session = self.session_arg(args)?;
                let command: ControlCommand =
                    serde_json::from_value(args.get("command").cloned().ok_or("缺少 command")?)
                        .map_err(|e| format!("command 非法: {e}"))?;
                let epoch = self.cached_epoch(session)?;
                self.hub
                    .control(&self.actor.clone(), session, epoch, &command)
                    .map_err(describe)?;
                Ok(format!("命令已受理: {command:?}（epoch={epoch}）"))
            }
            "pb_breakpoint_set" => {
                let session = self.session_arg(args)?;
                let address = arg_u64(args, "address")?;
                let label = args
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let epoch = self.cached_epoch(session)?;
                self.hub
                    .control(
                        &self.actor.clone(),
                        session,
                        epoch,
                        &ControlCommand::BreakpointSet { address, label },
                    )
                    .map_err(describe)?;
                Ok(format!("已在 0x{address:X} 下断点"))
            }
            "pb_script_load" => {
                let session = self.session_arg(args)?;
                let name = arg_str(args, "name")?;
                let source = arg_str(args, "source")?;
                let subscribe = args
                    .get("subscribe")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .filter_map(kind_from_name)
                            .collect()
                    })
                    .unwrap_or_default();
                let epoch = self.cached_epoch(session)?;
                let script = self
                    .hub
                    .load_script(
                        &self.actor.clone(),
                        session,
                        epoch,
                        name.as_str(),
                        source.as_str(),
                        subscribe,
                    )
                    .map_err(describe)?;
                let hash = self
                    .hub
                    .scripts(session)
                    .and_then(|all| all.last())
                    .map(|s| s.source_sha256.clone())
                    .unwrap_or_default();
                Ok(format!("脚本 {name} 已加载（id={script}，sha256={hash}）"))
            }
            "pb_events_query" => {
                let session = self.session_arg(args)?;
                let from = arg_u64(args, "from")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(256) as usize;
                let fold = args
                    .get("fold_repeats")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let (events, next, dropped) = self
                    .hub
                    .query_events(&self.actor.clone(), session, from, limit, fold)
                    .map_err(describe)?;
                let mut text = to_text(&events)?;
                text.push_str(&format!("\nnext={next} dropped={dropped}"));
                if dropped > 0 {
                    text.push_str(
                        "\n注意：dropped 非零，说明有事件因背压被丢弃；结论必须考虑这一前提。",
                    );
                }
                Ok(text)
            }
            "pb_trace_record" => {
                let session = self.session_arg(args)?;
                let action = arg_str(args, "action")?;
                let epoch = self.cached_epoch(session)?;
                match action.as_str() {
                    "start" => {
                        let profile = args
                            .get("profile")
                            .and_then(Value::as_str)
                            .unwrap_or("repeat+delta");
                        let path = self
                            .hub
                            .start_trace(&self.actor.clone(), session, epoch, profile)
                            .map_err(describe)?;
                        Ok(format!("开始录制: {path}"))
                    }
                    "stop" => {
                        let summary = self
                            .hub
                            .stop_trace(&self.actor.clone(), session, epoch)
                            .map_err(describe)?;
                        Ok(format!(
                            "停止录制: records={} events={} folded={} dropped={} truncated={}",
                            summary.records,
                            summary.events,
                            summary.repeat_folded,
                            summary.dropped,
                            summary.truncated
                        ))
                    }
                    other => Err(format!("action 只能是 start/stop，收到 {other}")),
                }
            }
            "pb_provenance" => {
                let session = self.session_arg(args)?;
                let lines = self.hub.provenance_of(session);
                Ok(lines.join("\n"))
            }
            "pb_capability_grant" => {
                let session = self.session_arg(args)?;
                let target_name = arg_str(args, "target_principal")?;
                let (kind, name) = target_name
                    .split_once(':')
                    .unwrap_or(("ai", target_name.as_str()));
                let target = if kind == "human" {
                    Principal::human(name)
                } else {
                    Principal::ai(name)
                };
                let requested: Vec<Capability> = args
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .filter_map(capability_from_name)
                            .collect()
                    })
                    .unwrap_or_default();
                let epoch = self.cached_epoch(session)?;
                let granted = self
                    .hub
                    .grant_capabilities(&self.actor.clone(), session, epoch, &target, &requested)
                    .map_err(describe)?;
                Ok(format!(
                    "已授权 {target}：{:?}。注意只有人类能执行本操作。",
                    granted.iter().map(Capability::name).collect::<Vec<_>>()
                ))
            }
            "pb_memory_read" => Err("M0 阶段内存读取依赖 agent 链路（Pin），尚未接通。\
请改用 pb_script_load 在目标进程内读取，或跟踪 docs/roadmap.md 的 M1。"
                .to_owned()),
            other => Err(format!(
                "工具 {other} 已声明但未实现（内部不一致，请报 issue）"
            )),
        }
    }

    fn session_arg(&self, args: &Value) -> std::result::Result<SessionId, String> {
        Ok(SessionId::new(arg_u64(args, "session")?))
    }

    fn cached_epoch(&self, session: SessionId) -> std::result::Result<u64, String> {
        self.epochs
            .get(&session.get())
            .copied()
            .ok_or_else(|| format!("会话 {session} 尚未在本 MCP 连接中建立，请先 attach 或 create"))
    }
}

fn error_response(id: Value, error: &ProtocolError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": error.code, "message": error.message}})
}

fn tool_error(message: String) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true
    })
}

fn to_text<T: serde::Serialize>(value: &T) -> std::result::Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| format!("序列化失败: {e}"))
}

fn describe(error: Error) -> String {
    match &error {
        Error::CapabilityDenied { .. } => {
            format!("{error}。这一步需要更强能力，请让人类用 pb_capability_grant 授权。")
        }
        Error::ControlConflict {
            holder,
            current_epoch,
            requested_epoch,
        } => format!(
            "{error}。控制权可能在别处被接管（当前持有者 {holder}，当前 epoch {current_epoch}，\
你手里的 epoch 是 {requested_epoch}）。请重新 attach 获取新 epoch 后重试。"
        ),
        Error::Script(_) => format!("{error}。注意：运行中的会话不接受新脚本，请先 pause。"),
        _ => error.to_string(),
    }
}

fn arg_str(args: &Value, key: &str) -> std::result::Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn arg_u64(args: &Value, key: &str) -> std::result::Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("缺少参数 {key} 或类型不是整数"))
}

fn kind_from_name(name: &str) -> Option<pinbridge_trace::event::EventKind> {
    (1u8..=12).find_map(|code| {
        let kind = pinbridge_trace::event::EventKind::from_code(code)?;
        (kind.name() == name).then_some(kind)
    })
}

fn capability_from_name(name: &str) -> Option<Capability> {
    Capability::ALL.into_iter().find(|c| c.name() == name)
}

/// 在 stdio 上跑服务循环，直到 EOF 或收到 `shutdown`。
pub fn serve_stdio<R: BufRead, W: Write>(
    server: &mut McpServer,
    input: R,
    mut output: W,
) -> Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let is_shutdown = line.contains("\"shutdown\"");
        if let Some(response) = server.handle(&line) {
            serde_json::to_writer(&mut output, &response)
                .map_err(|e| Error::Protocol(format!("写出响应失败: {e}")))?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        if is_shutdown {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> McpServer {
        let dir = std::env::temp_dir().join(format!("pb-mcp-{}", std::process::id()));
        McpServer::new(HubService::with_trace_dir(dir))
    }

    fn call(server: &mut McpServer, method: &str, params: Value) -> Value {
        server
            .handle(
                &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
            )
            .expect("应返回响应")
    }

    #[test]
    fn initialize_reports_server_info_and_instructions() {
        let mut server = server();
        let response = call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "cline"}, "protocolVersion": "2025-06-18"}),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "pinbridge-mcp");
        assert!(response["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("能力集"));
        assert!(server.is_initialized());
        assert_eq!(server.actor().name(), "cline");
    }

    #[test]
    fn tools_list_only_shows_what_the_actor_may_call() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "ai"}}),
        );
        let response = call(&mut server, "tools/list", json!({}));
        let names: Vec<&str> = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        // AI 默认只读：看不到授权/脚本类工具
        assert!(names.contains(&"pb_session_list"));
        assert!(names.contains(&"pb_events_query"));
        assert!(
            !names.contains(&"pb_capability_grant"),
            "只读主体不应看到授权工具"
        );
        assert!(
            !names.contains(&"pb_script_load"),
            "只读主体不应看到脚本注入工具"
        );
    }

    #[test]
    fn unknown_tool_and_method_are_reported_with_correct_codes() {
        let mut server = server();
        call(&mut server, "initialize", json!({}));
        let response = call(&mut server, "tools/call", json!({"name": "pb_nope"}));
        assert_eq!(response["error"]["code"], -32602);
        let response = call(&mut server, "nonsense/method", json!({}));
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn parse_error_and_notifications() {
        let mut server = server();
        let response = server.handle("{not json").unwrap();
        assert_eq!(response["error"]["code"], -32700);
        // 通知没有 id → 不响应
        assert!(server
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string())
            .is_none());
    }

    #[test]
    fn read_only_actor_cannot_create_a_session_and_gets_a_hint() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "ai"}}),
        );
        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_session_create", "arguments": {"image_path": "TARGET.exe"}}),
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("能力不足"), "实际: {text}");
        assert!(
            text.contains("pb_capability_grant"),
            "错误里必须给出下一步建议"
        );
    }

    #[test]
    fn full_readonly_flow_create_attach_query() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "ai"}}),
        );
        // 人类操作员先建会话并授权给 AI（直接走 Hub，模拟工作台/CLI 侧）
        let human = Principal::human("alice");
        let session = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .unwrap();
        server
            .hub
            .attach(&human, session, 4242, "TARGET.exe")
            .unwrap();
        let epoch = server.hub.epoch_of(session).unwrap();
        server
            .hub
            .grant_capabilities(
                &human,
                session,
                epoch,
                &server.actor.clone(),
                &Capability::ALL,
            )
            .unwrap();
        server.capabilities = server.hub.capabilities_of(&server.actor.clone());
        server.epochs.insert(session.get(), epoch);

        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_session_list"}),
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("TARGET.exe"));

        for _ in 0..50 {
            server
                .hub
                .push_event(
                    session,
                    pinbridge_trace::event::EventKind::SyscallEnter,
                    100,
                    0x2000,
                    &[(0, 0x40)],
                )
                .unwrap();
        }
        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_events_query", "arguments": {"session": session.get(), "from": 1, "limit": 64}}),
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("repeat"), "折叠后应出现 repeat 记录: {text}");
        assert!(text.contains("dropped=0"));

        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_provenance", "arguments": {"session": session.get()}}),
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("grant_capabilities"));
    }

    #[test]
    fn stale_epoch_from_a_takeover_surfaces_as_an_actionable_error() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "ai"}}),
        );
        let human = Principal::human("alice");
        let session = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .unwrap();
        server.hub.attach(&human, session, 1, "").unwrap();
        let epoch = server.hub.epoch_of(session).unwrap();
        server
            .hub
            .grant_capabilities(
                &human,
                session,
                epoch,
                &server.actor.clone(),
                &Capability::ALL,
            )
            .unwrap();
        server.capabilities = server.hub.capabilities_of(&server.actor.clone());
        server.epochs.insert(session.get(), epoch);

        // 人类接管 → epoch 推进
        server.hub.takeover(&human, session).unwrap();

        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_control", "arguments": {"session": session.get(), "command": {"cmd": "continue"}}}),
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("控制权"), "实际: {text}");
        assert!(text.contains("重新 attach"), "错误里必须告诉 AI 怎么办");
        assert_eq!(response["result"]["isError"], true);
    }

    #[test]
    fn serve_stdio_reads_until_shutdown() {
        let mut server = server();
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"shutdown\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}\n"
        );
        let mut output = Vec::new();
        serve_stdio(&mut server, input.as_bytes(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "shutdown 之后不再处理消息，实际: {lines:?}");
        assert!(lines[0].contains("pinbridge-mcp"));
        // shutdown 的响应是 result:{}}，用它自己的 id 来确认"第三条正是 shutdown 的回应"
        assert!(lines[2].contains("\"id\":3"), "实际: {}", lines[2]);
    }

    #[test]
    fn helper_mappers_cover_all_names() {
        assert_eq!(capability_from_name("mem_read"), Some(Capability::MemRead));
        assert_eq!(capability_from_name("nope"), None);
        assert_eq!(
            kind_from_name("syscall_enter"),
            Some(pinbridge_trace::event::EventKind::SyscallEnter)
        );
        assert_eq!(kind_from_name("nope"), None);
    }
}
