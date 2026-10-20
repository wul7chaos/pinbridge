//! `operator_mcp` —— 「人类座位」+ 「MCP 服务」合体的一个进程。
//!
//! 为什么需要它：`pinbridge-mcp` 的 actor **永远是 AI**（只读基线
//! `[mem_read, trace_record]`），而 `create_session` / `attach` /
//! `grant_capabilities` 都要 `process_control` —— **AI 不能给自己扩权**。
//! 桌面工作台是这个「人类座位」的图形版，本示例是它的**常驻版**：
//! 先把人的那几步做完，再把同一个 Hub 以 MCP stdio 服务暴露给 AI。
//!
//! 和 `headless_operator` 的区别：那个跑一次就退出；这个**留在原地当 MCP 服务**，
//! 于是 DSH / Cline 这类 stdio 客户端拿到的工具**一开始就带满能力**。
//! 授权动作因此仍然是一次显式的人类行为 —— **人类亲手用它启动、并给出目标 PID**。
//!
//! 用法：
//! ```powershell
//! $env:PINBRIDGE_AGENT_PORT_FILE = "$env:TEMP\pb-agent-port.txt"
//! operator_mcp.exe --pid 1508 --image "D:\Game\Path of Exile\PathOfExile.exe"
//! ```
//!
//! 约定：**stdout 只允许 JSON-RPC**，所有诊断打印走 stderr。

use std::io::{BufRead, Write};

use pinbridge_client::transport::TcpTransport;
use pinbridge_client::AgentClient;
use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::principal::Principal;
use pinbridge_core::session::TargetSpec;
use pinbridge_hub::HubService;
use pinbridge_mcp::link::ClientLink;
use pinbridge_mcp::McpServer;
use serde_json::Value;

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// 从一帧 JSON-RPC 里抠出 `clientInfo.name`（只在 `initialize` 上有）。
fn client_name_of(frame: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(frame).ok()?;
    if parsed.get("method").and_then(Value::as_str) != Some("initialize") {
        return None;
    }
    parsed
        .get("params")?
        .get("clientInfo")?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "operator_mcp —— 人类座位 + MCP stdio 服务（同一个 Hub）\n\n\
             用法: operator_mcp --pid <PID> [--image <路径>] [--port-file <文件>] [--trace-dir <目录>]\n\n\
             \x20 --pid         目标进程号（必填）\n\
             \x20 --image       目标映像路径（默认 TARGET.exe）\n\
             \x20 --port-file   agent 端口文件（默认读 PINBRIDGE_AGENT_PORT_FILE）\n\
             \x20 --trace-dir   轨迹目录（默认系统临时目录下的 pinbridge-traces）\n\n\
             stdout 只输出 JSON-RPC；诊断全部走 stderr。"
        );
        return Ok(());
    }

    // `--pid` **可选**：人类可以不在启动时给出目标（刚重启过、游戏还没开是常态）。
    // 不给就只建会话、不登记附加 —— 之后的 `pb_session_attach` 由 AI 带着**真实** pid 调用
    // （这个示例在 initialize 时已经把 ALL 授给它了，所以它能自己附加）。
    let pid: Option<u32> = match arg_value(&args, "--pid") {
        Some(raw) => Some(
            raw.parse()
                .map_err(|e| format!("--pid 不是合法进程号: {e}"))?,
        ),
        None => None,
    };
    let image = arg_value(&args, "--image").unwrap_or_else(|| "TARGET.exe".to_owned());
    let trace_dir = arg_value(&args, "--trace-dir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("pinbridge-traces"));
    let port_file =
        arg_value(&args, "--port-file").or_else(|| std::env::var("PINBRIDGE_AGENT_PORT_FILE").ok());

    let mut hub = HubService::with_trace_dir(trace_dir);
    let human = Principal::human("operator");

    // 给后面那条"仅登记"的提示用：`--pid` 没给时也要能说出人话。
    let pid_msg = pid.map_or_else(|| "（未指定）".to_owned(), |v| v.to_string());

    // ① 建会话（人做）
    let session = hub.create_session(&human, TargetSpec::launch(image.clone(), "x86_64"))?;
    eprintln!("✓ 会话 {session} 已建（操作员 {human}）");

    // ② 附加登记（人做；只有给了 --pid 才做）
    let epoch = match pid {
        Some(pid) => {
            hub.attach(&human, session, pid, &image)?;
            let epoch = hub.epoch_of(session)?;
            eprintln!("✓ 已登记附加 pid={pid}（epoch={epoch}）");
            epoch
        }
        None => {
            let epoch = hub.epoch_of(session)?;
            eprintln!(
                "· 未给 --pid：只建了会话（epoch={epoch}），尚未附加。\
                 用 pb_session_attach(pid=<真实 pid>) 指定目标。"
            );
            epoch
        }
    };

    // ③ 连 agent 链路（注入由 scripts/inject-agent.ps1 负责，这里只按端口文件去连）
    match port_file {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("读不到端口文件 {path}: {e}"))?;
            let endpoint = text.trim().to_owned();
            // 链路连不上**不能**让服务退出：端口文件很可能是上一次注入留下的陈旧值，
            // 而"服务活着 + 工具诚实地报错"比"进程直接死掉"好排查得多。
            match TcpTransport::connect(endpoint.as_str()) {
                Ok(transport) => {
                    // 链路侧能力必须是 ALL：只读基线的话 agent 不会往这边推
                    // 插桩事件，`pb_trace_record` 就会录到 events=0（实测过）。
                    let all = Capability::ALL
                        .iter()
                        .fold(CapabilitySet::empty(), |set, cap| set.with(*cap));
                    let link = ClientLink::new(AgentClient::new(transport).with_capabilities(all));
                    match hub.attach_link(&human, session, Box::new(link)) {
                        Ok(described) => eprintln!("✓ 链路已连上 {endpoint}（{described}）"),
                        Err(e) => eprintln!("⚠ 链路已连上但 attach_link 失败：{e}"),
                    }
                }
                Err(e) => eprintln!(
                    "⚠ **仅登记**：连不上 {endpoint}（{e}）\n\
                     \x20  两种可能，按顺序查：\n\
                     \x20  ① 目标进程已退出（pid={pid_msg}）—— 重启电脑后 pid 必然变了；\n\
                     \x20  ② 端口文件是上一次注入留下的残留。\n\
                     \x20  确认目标在跑之后，重跑 scripts/inject-agent.ps1 -TargetPid <新PID> -LinkPortFile <同一个文件>。"
                ),
            }
        }
        None => {
            eprintln!("⚠ 没有端口文件：**仅登记**，链路工具会失败（先跑 inject-agent.ps1）");
        }
    }

    // ④ 起 MCP 服务；在 `initialize` 到达时把 ALL 授给这个 AI 主体的**真实名字**
    //    （名字由客户端 `clientInfo.name` 决定，所以必须在看到 initialize 之后再授；
    //     而 `initialize` 自己会重取一次能力集，所以授名要赶在 server.handle 之前）。
    let mut server = McpServer::new(hub);
    // 会话是人在外面建的，MCP 侧不知道它的 epoch；不补这一步，
    // pb_trace_record / pb_control 会报「会话未在本 MCP 连接中建立」。
    server.adopt_session(session, epoch);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    eprintln!("✓ MCP 服务就绪（stdio）；等待客户端 initialize");

    for line in stdin.lock().lines() {
        let frame = line?;
        if frame.trim().is_empty() {
            continue;
        }
        if let Some(name) = client_name_of(&frame) {
            let ai = Principal::ai(name);
            match server
                .hub_mut()
                .grant_capabilities(&human, session, epoch, &ai, &Capability::ALL)
            {
                Ok(granted) => eprintln!("✓ 已授权给 {ai}：{granted:?}"),
                Err(e) => eprintln!("✗ 授权失败（{ai}）：{e}"),
            }
        }
        if let Some(response) = server.handle(&frame) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
