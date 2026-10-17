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

/// 从端口文件连上 agent，返回"链路 + 一句给人看的话"。
///
/// # 为什么返回值里带一句话，而不是干脆 `Result<()>`
///
/// `pb_session_attach` 以前一律回"已附加" —— 而 `HubService::attach` **只登记**
/// （见 `docs/dev/attach-capability-gaps.md` 的 G2）。链路连不上时**不该**让 attach
/// 整体失败（登记本身有用），但**必须**把"其实没连上"说出来，否则那句"已附加"就是谎话。
///
/// `PINBRIDGE_AGENT_PORT_FILE` 指向 `pb_link_start`（agent 侧导出）写出来的端口文件。
fn connect_agent_link(
    port_file: Option<&std::path::Path>,
    capabilities: pinbridge_core::capability::CapabilitySet,
) -> std::result::Result<(Box<dyn pinbridge_hub::TargetLink>, String), String> {
    let Some(path) = port_file else {
        return Err(
            "没有配置 agent 端口文件（设 PINBRIDGE_AGENT_PORT_FILE 指向 pb_link_start 写出的文件后才能真正连上）"
                .to_owned(),
        );
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读不到端口文件 {}（{error}）", path.display()))?;
    let addr: std::net::SocketAddr = text
        .trim()
        .parse()
        .map_err(|_| format!("端口文件内容不是 127.0.0.1:PORT（{:?}）", text.trim()))?;
    let transport = pinbridge_client::transport::TcpTransport::connect(addr)
        .map_err(|error| format!("连不上 {addr}（{error}）"))?;
    // **必须声明能力集**：agent 按客户端声明的这一份来拦截（不声明 = 0 = 什么都做不了，
    // 实测会得到「能力不足: 需要 MemRead，当前持有 CapabilitySet(0)」）。
    // 声明的是**这个 actor 在 Hub 侧的真实能力**，不是"给什么都不吃亏"的 ALL。
    let link = crate::link::ClientLink::new(
        pinbridge_client::client::AgentClient::new(transport).with_capabilities(capabilities),
    );
    Ok((
        Box::new(link),
        format!("链路已连上 {addr}（agent 在自己进程里应答）"),
    ))
}

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

                // 登记之后再试一次"真的连上"（G3）。连不上不算失败 ——
                // 但回话里必须写清"仅登记"，否则"已附加"就是谎话（G2）。
                let port_file =
                    std::env::var_os("PINBRIDGE_AGENT_PORT_FILE").map(std::path::PathBuf::from);
                let link_note = match connect_agent_link(port_file.as_deref(), self.capabilities) {
                    Ok((link, note)) => {
                        match self.hub.attach_link(&self.actor.clone(), session, link) {
                            Ok(described) => format!("；{note}（{described}）"),
                            Err(error) => format!(
                                "；**仅登记**：链路连上了但挂到会话失败（{}）",
                                describe(error)
                            ),
                        }
                    }
                    Err(reason) => format!("；**仅登记**：{reason}"),
                };
                Ok(format!(
                    "已附加到 pid={pid}（会话 {session}，epoch={epoch}）{link_note}"
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
            "pb_registers" => {
                let session = self.session_arg(args)?;
                let thread_arg = arg_u64(args, "thread")?;
                // 线程号在协议里是 u32；超出范围说明调用方搞错了对象
                // （例如把地址当成线程号传进来），要明确拒绝而不是截断。
                let thread = u32::try_from(thread_arg).map_err(|_| {
                    format!(
                        "thread={thread_arg} 超出 u32 范围（线程号取自事件里的 thread_id，不是地址）"
                    )
                })?;
                let values = self
                    .hub
                    .read_registers(&self.actor.clone(), session, thread)
                    .map_err(describe)?;
                if values.is_empty() {
                    // 空是"不该出现"的一种：agent 要么给出快照，要么报错。
                    // 真出现了也要说清楚，不能让它看起来像"这个线程没有寄存器"。
                    return Ok(format!(
                        "线程 {thread} 的寄存器快照是空的——这不该发生（要么有值、\
                         要么报错），请把它当作缺陷报告。"
                    ));
                }
                // 快照是"最近一次探针命中时"的，不是"此刻"。这句话必须跟着数据
                // 一起回给调用方：AI 很容易把它当成实时值去做时序推断。
                let mut text =
                    format!("线程 {thread} 最近一次探针命中时的寄存器（**不是此刻的值**）：\n");
                for (name, value) in &values {
                    text.push_str(&format!("  {name:<4} {value:#018x}\n"));
                }
                Ok(text)
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
            "pb_modules" => {
                let session = self.session_arg(args)?;
                let (modules, skipped) = self
                    .hub
                    .list_modules(&self.actor.clone(), session)
                    .map_err(describe)?;
                if modules.is_empty() {
                    // 空列表是有含义的（"一个模块都没有"）。但更常见的原因是
                    // 链路到了却拿不到加载器链——这时必须让调用方看见 skipped
                    // 与这条提示，而不是把空列表当成结论。
                    return Ok(format!(
                        "没有列出任何模块（跳过 {skipped} 个条目）。\
                         目标可能尚未完成加载，或加载器链不可读；\
                         不要据此认为目标没有加载模块。"
                    ));
                }
                let mut text = String::from("模块（加载顺序，基址 + 大小）：\n");
                for module in &modules {
                    text.push_str(&format!(
                        "  {:#018x} +{:#010x}  {}\n",
                        module.base,
                        u64::from(module.size),
                        module.name
                    ));
                }
                if skipped != 0 {
                    // 不完整必须说出来：这份列表是**快照**，
                    // 加载器链正在被修改时会出现半初始化条目。
                    text.push_str(&format!(
                        "注意：跳过 {skipped} 个不合法条目，列表可能不完整\
                         （快照读法不持 loader lock）。"
                    ));
                }
                Ok(text)
            }
            "pb_memory_read" => {
                let session = self.session_arg(args)?;
                let address = arg_u64(args, "address")?;
                // 长度按 u32 转换：一次读取的上限由 agent 侧把关（超限会**明确拒绝**
                // 而不是悄悄截短），所以这里不做隐式收敛，只做范围转换。
                let length = u32::try_from(arg_u64(args, "length")?).map_err(|_| {
                    "length 超出 u32 范围（单次读取按字节计；大块请用轨迹或脚本快照）"
                })?;
                let bytes = self
                    .hub
                    .read_memory(&self.actor.clone(), session, address, length)
                    .map_err(describe)?;
                Ok(format!(
                    "{address:#x} 起 {length} 字节：{}\n（回读长度与请求一致，两端都核对过）",
                    to_hex(&bytes)
                ))
            }
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

/// 把字节序列写成十六进制（工具结果里给人/AI 看的形式）。
///
/// 不用 `{:02x?}` 直接打 `Vec<u8>`：那会输出 `[144, 204, …]`，
/// 十六进制是分析场景里唯一不用换算就看得懂的写法。
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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

    /// 一条测试用链路：按预设字节回答读内存。
    #[derive(Debug)]
    struct FakeLink(Vec<u8>);

    impl pinbridge_hub::TargetLink for FakeLink {
        fn read_memory(
            &mut self,
            _address: u64,
            length: u32,
        ) -> pinbridge_core::error::Result<Vec<u8>> {
            let mut out = self.0.clone();
            out.resize(length as usize, 0);
            Ok(out)
        }

        fn list_modules(
            &mut self,
        ) -> pinbridge_core::error::Result<(Vec<pinbridge_proto::agent_link::ModuleInfo>, u32)>
        {
            Ok((
                vec![pinbridge_proto::agent_link::ModuleInfo {
                    name: "TARGET.exe".into(),
                    base: 0x1_4000_0000,
                    size: 0x2000,
                    entry_point: 0x1_4000_1000,
                }],
                0,
            ))
        }

        fn read_registers(
            &mut self,
            thread: u32,
        ) -> pinbridge_core::error::Result<Vec<(String, u64)>> {
            // 线程 7 有快照；其它线程给出"没有快照"的**正常失败**——
            // 这样用例能覆盖"有值"与"正常地没有"两条路径。
            if thread == 7 {
                return Ok(vec![
                    ("rip".into(), 0x1_4000_1234),
                    ("rsp".into(), 0x20_FF00),
                ]);
            }
            Err(pinbridge_core::error::Error::TargetUnavailable(format!(
                "pb_read_registers: 线程 {thread} 没有快照（有快照的线程：[7]）\
                 。快照只在探针命中时由 Pin 工具捕获，没命中过就没有"
            )))
        }

        fn describe(&self) -> String {
            "测试用假链路".into()
        }
    }

    #[test]
    fn registers_tool_returns_values_and_says_what_is_missing() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "alice"}}),
        );
        let human = Principal::human("operator");
        let id = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .expect("人类建会话应当成功");
        server
            .hub
            .attach(&human, id, 4242, "TARGET.exe --demo")
            .expect("附加应当成功");
        server
            .hub
            .attach_link(&human, id, Box::new(FakeLink(vec![0x90])))
            .expect("挂链路应当成功");
        // 给人授足以读寄存器（AI 默认只有只读基线，而 mem_read 就在基线里）；
        // 这里之所以显式授权：会话是**人类**建的，但 MCP 请求的 actor 是 initialize
        // 报上来的名字，两边的能力集是分开的。
        server
            .hub
            .grant_capabilities(
                &human,
                id,
                server.hub.epoch_of(id).unwrap(),
                &human,
                &[Capability::MemRead],
            )
            .expect("授权应当成功");

        // 1) 工具在清单里（能力足够时）
        let listed = call(&mut server, "tools/list", json!({}));
        let names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        assert!(
            names.contains(&"pb_registers".to_owned()),
            "实际: {names:?}"
        );

        // 2) 有快照的线程：值必须逐个打出来，而且要说清"这不是此刻的值"
        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_registers", "arguments": {"session": id.get(), "thread": 7}}),
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("rip"), "实际: {text}");
        // 值按 16 位十六进制补零打出来（`0x` + 16 位），便于逐字节比对。
        assert!(text.contains("0x0000000140001234"), "实际: {text}");
        assert!(
            text.contains("不是此刻"),
            "必须带上『最近一次而不是此刻』这句话（否则会被当成实时值）: {text}"
        );

        // 3) 没有快照的线程：这是**正常失败**，错误里要给出"有快照的线程"
        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_registers", "arguments": {"session": id.get(), "thread": 3}}),
        );
        assert_eq!(
            response["result"]["isError"], true,
            "工具级错误：{response}"
        );
        let err = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(err.contains("没有快照"), "实际: {err}");
        assert!(err.contains("[7]"), "要说清哪些线程有快照: {err}");

        // 4) 线程号超出 u32：明确拒绝，而不是截断
        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_registers", "arguments": {"session": id.get(), "thread": 5_000_000_000u64}}),
        );
        assert_eq!(
            response["result"]["isError"], true,
            "工具级错误：{response}"
        );
        let err = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(err.contains("超出 u32"), "实际: {err}");
        println!("✓ 寄存器工具：有快照给值、没快照说清、越界拒绝");
    }
    #[test]
    fn modules_tool_prints_base_and_path_after_link_attach() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "alice"}}),
        );
        let human = Principal::human("operator");
        let id = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .expect("人类建会话应当成功");
        server
            .hub
            .attach(&human, id, 4242, "TARGET.exe --demo")
            .expect("附加应当成功");
        server
            .hub
            .attach_link(&human, id, Box::new(FakeLink(vec![0x90])))
            .expect("挂链路应当成功");

        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_modules", "arguments": {"session": id.get()}}),
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("0x0000000140000000"), "应当带出基址：{text}");
        assert!(text.contains("TARGET.exe"), "应当带出模块路径：{text}");
        println!("✓ 模块列表：{}", text.replace('\n', " | "));
    }
    /// 建会话、附加、并把 `Capability::ALL` 授给 MCP 的 actor（AI）。
    ///
    /// 为什么要授权：`pb_session_attach` 需要 `process_control`，而 AI 默认只有只读基线
    /// `[mem_read, trace_record]` —— 这是**设计**（人建会话、人授权、AI 才动手），
    /// 所以用例必须自己把这一步演出来，否则测到的是闸门而不是接线。
    fn attached_session(server: &mut McpServer) -> u64 {
        call(
            server,
            "initialize",
            json!({"clientInfo": {"name": "alice"}}),
        );
        let human = Principal::human("operator");
        let id = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .expect("人类建会话应当成功");
        server
            .hub
            .attach(&human, id, 4242, "TARGET.exe")
            .expect("人类附加应当成功");
        let epoch = server.hub.epoch_of(id).expect("取 epoch");
        server
            .hub
            .grant_capabilities(&human, id, epoch, &server.actor.clone(), &Capability::ALL)
            .expect("人类授权应当成功");
        server.capabilities = server.hub.capabilities_of(&server.actor.clone());
        server.epochs.insert(id.get(), epoch);
        id.get()
    }

    fn attach_text(server: &mut McpServer, session: u64) -> (Value, String) {
        let response = call(
            server,
            "tools/call",
            json!({"name": "pb_session_attach", "arguments": {"session": session, "pid": 4242}}),
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (response, text)
    }

    /// G2 + G3：`pb_session_attach` 的回话必须**诚实**。
    ///
    /// 三种情况合并在一条用例里，因为端口文件是靠进程级环境变量传的 ——
    /// 拆成三条并行跑会互相踩（本仓库的用例不保证串行）。
    ///
    /// * 没有端口文件 ⇒ "**仅登记**"（以前一律回"已附加"，而目标里什么都没有）；
    /// * 端口文件指向活着的监听 ⇒ "链路已连上"（G3 的接线真的接上了）；
    /// * 端口文件指向没人听的端口 ⇒ 仍说"**仅登记**"并带上原因（不假装成功）。
    #[test]
    fn attach_reports_honestly_whether_the_agent_link_is_live() {
        const KEY: &str = "PINBRIDGE_AGENT_PORT_FILE";
        let mut server = server();
        let session = attached_session(&mut server);

        // ① 没有端口文件
        std::env::remove_var(KEY);
        let (response, text) = attach_text(&mut server, session);
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "attach 本身应当成功（登记是有用的）：{response}"
        );
        assert!(
            text.contains("已附加到 pid=4242"),
            "回话里应有登记结果：{text}"
        );
        assert!(
            text.contains("仅登记"),
            "没有端口文件时必须写清「仅登记」，否则「已附加」就是谎话：{text}"
        );
        assert!(
            text.contains(KEY),
            "回话应当告诉使用者怎么才能真正连上：{text}"
        );
        println!("✓ ① 没有端口文件：{text}");

        // ② 端口文件指向活着的监听（裸 TcpListener 冒充 agent 侧：握手由内核完成）
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占一个回环端口");
        let live = listener.local_addr().expect("取地址");
        let dir = std::env::temp_dir().join(format!("pb-link-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let live_file = dir.join("agent-port.live.txt");
        std::fs::write(&live_file, format!("{live}\n")).expect("写端口文件");
        std::env::set_var(KEY, &live_file);
        let (response, text) = attach_text(&mut server, session);
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "attach 应当成功：{response}"
        );
        assert!(
            text.contains("链路已连上"),
            "端口文件有效时必须报「链路已连上」：{text}"
        );
        assert!(
            !text.contains("仅登记"),
            "连上了就不该再说「仅登记」：{text}"
        );
        println!("✓ ② 端口文件有效：{text}");

        // ③ 端口文件指向没人听的端口（占一个立刻关掉）
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占端口");
            listener.local_addr().expect("取地址")
        };
        let dead_file = dir.join("agent-port.dead.txt");
        std::fs::write(&dead_file, format!("{dead}\n")).expect("写端口文件");
        std::env::set_var(KEY, &dead_file);
        let (response, text) = attach_text(&mut server, session);
        std::env::remove_var(KEY);
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "attach 应当成功：{response}"
        );
        assert!(
            text.contains("仅登记"),
            "连不上时也必须说清「仅登记」：{text}"
        );
        println!("✓ ③ 连不上：{text}");
    }

    /// **一次性端到端**：人建会话并授权 → AI 走 MCP `pb_session_attach` → 真的连上目标里的
    /// agent → `pb_modules` 的模块来自目标自己的加载器链。
    ///
    /// # 为什么必须在进程内做
    ///
    /// MCP 的 actor 是 **AI**（只读基线），而 `session_create` / `attach` 都要
    /// `process_control` —— **AI 不能给自己扩权**（Hub 直接拒绝，这正是设计）。
    /// 所以"人"和"AI"必须共用一个 Hub 实例：这也正是工作台的真实形态
    /// （人在这头点授权，AI 在那头调工具）。
    ///
    /// # 为什么"目标侧"能在一个测试进程里成立
    ///
    /// `pb_link_start` 起的是**进程内回环服务**，测试进程就扮演目标进程 —— 同一个导出、
    /// 同一条服务线程、同一套帧协议，不是另写一套假实现。
    #[test]
    fn end_to_end_human_grants_then_ai_attaches_and_reads_modules_over_the_real_link() {
        // ① 目标侧：起真链路
        let dir = std::env::temp_dir().join(format!("pb-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let port_file = dir.join("agent-port.txt");
        // 用安全入口（`start_link_server`）而不是 C 导出：本 crate 是 `forbid(unsafe_code)`，
        // 而这条路径与 C 导出走的是**同一个**静态服务与线程，只是少了解 C 指针那一步。
        let addr = pinbridge_agent::exports::start_link_server(&port_file.to_string_lossy())
            .expect("agent 侧链路应当启动成功");
        println!("✓ ① agent 链路：{addr}");
        // agent **自己**也有一份能力集（Hub 的能力集是另一份，两边都要有）：
        // 不下发的话链路工具会以「能力不足: 需要 MemRead，当前持有 CapabilitySet(0)」被拒。
        let bits = Capability::read_only_baseline().bits();
        let set_code = pinbridge_agent::exports::pb_set_capabilities(bits);
        let readback = pinbridge_agent::exports::pb_get_capabilities();
        println!("✓ ① agent 能力：下发 {bits:#x} 返回 {set_code}，读回 {readback:#x}");
        assert_eq!(set_code, 0, "下发 agent 能力应当成功");
        std::env::set_var("PINBRIDGE_AGENT_PORT_FILE", &port_file);

        // ② 人建会话 + 授权（同一个 Hub）
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "e2e"}}),
        );
        let human = Principal::human("operator");
        let id = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .expect("人类建会话");
        server
            .hub
            .attach(&human, id, 4242, "TARGET.exe")
            .expect("人类附加");
        let epoch = server.hub.epoch_of(id).expect("epoch");
        server
            .hub
            .grant_capabilities(&human, id, epoch, &server.actor.clone(), &Capability::ALL)
            .expect("人类授权");
        server.capabilities = server.hub.capabilities_of(&server.actor.clone());
        server.epochs.insert(id.get(), epoch);

        // ③ AI 走 MCP 工具 attach：回话必须说"链路已连上"
        let (response, text) = attach_text(&mut server, id.get());
        std::env::remove_var("PINBRIDGE_AGENT_PORT_FILE");
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "attach 不该失败：{response}"
        );
        assert!(
            text.contains("链路已连上"),
            "应当真的连上 agent 链路（而不是仅登记）：{text}"
        );
        println!("✓ ③ attach：{text}");

        // ④ 链路工具：模块列表必须来自目标侧（本进程 = 目标进程），不是假数据
        let response = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_modules", "arguments": {"session": id.get()}}),
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "pb_modules 应当成功：{response}"
        );
        println!("✓ ④ pb_modules：{text}");
        assert!(
            text.to_lowercase().contains("ntdll") || text.contains(".exe"),
            "模块列表看着不像真实数据（既没有 ntdll 也没有 .exe）：{text}"
        );
    }

    #[test]
    fn memory_read_returns_hex_once_a_link_is_attached() {
        let mut server = server();
        call(
            &mut server,
            "initialize",
            json!({"clientInfo": {"name": "alice"}}),
        );
        // 会话由**人类**建：MCP 的参与者是 AI，而 AI 建会话需要 ProcessControl
        // （它默认只有只读基线 257）——这正是设计要的人机分工。
        // 这里把"人类那一侧"直接演出来，然后让 AI 去读内存。
        let human = Principal::human("operator");
        let id = server
            .hub
            .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
            .expect("人类建会话应当成功");
        server
            .hub
            .attach(&human, id, 4242, "TARGET.exe --demo")
            .expect("附加应当成功");
        let session = id.get();

        // 先：没挂链路 → 错误必须**点明缺链路**（这是第 9 轮发现的那个缺口）
        let before = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_memory_read", "arguments": {
                "session": session, "address": 0x1000, "length": 4
            }}),
        );
        let before_text = before["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            before_text.contains("链路"),
            "没挂链路时必须说清缺什么，实际: {before_text}"
        );

        // 再：挂上链路 → 工具应当回出十六进制并把长度说清
        server
            .hub
            .attach_link(&human, id, Box::new(FakeLink(vec![0x90, 0xcc, 0x48, 0x89])))
            .expect("挂链路应当成功");
        let after = call(
            &mut server,
            "tools/call",
            json!({"name": "pb_memory_read", "arguments": {
                "session": session, "address": 0x1000, "length": 4
            }}),
        );
        let after_text = after["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            after_text.contains("90cc4889"),
            "应当回出十六进制字节，实际: {after_text}"
        );
        assert!(
            after_text.contains("0x1000"),
            "应当带出起始地址，实际: {after_text}"
        );
        println!("✓ 读内存：{after_text}");
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
