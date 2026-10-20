//! `headless_operator` —— 没有界面时的"人类操作位"。
//!
//! 为什么需要它：`pinbridge-mcp` 的 actor **永远是 AI**（默认只有只读基线），
//! 而 `create_session` / `attach` / `grant_capabilities` 都要求 `process_control`
//! —— **AI 不能给自己扩权**。桌面工作台就是这个"人类座位"；本示例是它的
//! 命令行等价物，用来在没有界面（CI、远程机、脚本化排查）的场合把
//! 「附加 → 连 agent 链路 → 授权给 AI → 真正读目标」一次跑完。
//!
//! 它自己**不做**注入：注入由 `scripts/inject-agent.ps1` 负责，
//! 两者靠 `PINBRIDGE_AGENT_PORT_FILE` 指向的端口文件对接。
//!
//! 用法：
//! ```powershell
//! $env:PINBRIDGE_AGENT_PORT_FILE = "$env:TEMP\pb-agent-port.txt"
//! cargo run -p pinbridge-mcp --example headless_operator -- <PID> <IMAGE_PATH> [ADDR_HEX] [LEN]
//! ```

use pinbridge_client::transport::TcpTransport;
use pinbridge_client::AgentClient;
use pinbridge_core::capability::Capability;
use pinbridge_core::principal::Principal;
use pinbridge_core::session::TargetSpec;
use pinbridge_hub::HubService;
use pinbridge_mcp::link::ClientLink;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let pid: u32 = args.next().ok_or("缺少 PID")?.parse()?;
    let image = args.next().unwrap_or_else(|| "TARGET.exe".to_owned());
    let address = args
        .next()
        .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16))
        .transpose()?;
    let length: u32 = args
        .next()
        .map(|s| s.parse::<u32>())
        .transpose()?
        .unwrap_or(32);

    let mut hub = HubService::with_trace_dir(std::env::temp_dir().join("pinbridge-traces"));
    let human = Principal::human("operator");

    // ① 建会话（人做）
    let session = hub.create_session(&human, TargetSpec::launch(image.clone(), "x86_64"))?;
    println!("✓ 会话 {session} 已建（操作员 {human}）");

    // ② 附加登记（人做）
    hub.attach(&human, session, pid, &image)?;
    let epoch = hub.epoch_of(session)?;
    println!("✓ 已登记附加 pid={pid}（epoch={epoch}）");

    // ③ 连 agent 链路（注入是另一个进程干的事，这里只按端口文件去连）
    let port_file = std::env::var("PINBRIDGE_AGENT_PORT_FILE").map_err(|_| {
        "没有 PINBRIDGE_AGENT_PORT_FILE：先跑 scripts/inject-agent.ps1 -LinkPortFile <文件>"
    })?;
    let endpoint = std::fs::read_to_string(&port_file)?;
    let endpoint = endpoint.trim().to_owned();
    let transport = TcpTransport::connect(endpoint.as_str())?;
    let link = ClientLink::new(
        AgentClient::new(transport).with_capabilities(Capability::read_only_baseline()),
    );
    let described = hub.attach_link(&human, session, Box::new(link))?;
    println!("✓ 链路已连上 {endpoint}（{described}）");

    // ④ 授权给 AI（人做；AI 自己做不到这一步）
    let ai = Principal::ai("cline");
    let granted = hub.grant_capabilities(&human, session, epoch, &ai, &Capability::ALL)?;
    println!("✓ 已授权给 {ai}：{granted:?}");

    // ⑤ 用 AI 的身份真的调一次需要链路的工具
    let (modules, skipped) = hub.list_modules(&ai, session)?;
    println!("✓ 模块 {} 个（跳过 {skipped} 条）", modules.len());
    for m in modules.iter().take(6) {
        println!("    {:#018x} +{:#010x}  {}", m.base, m.size, m.name);
    }

    if let Some(addr) = address {
        let bytes = hub.read_memory(&ai, session, addr, length)?;
        println!("✓ 读 {addr:#x} 共 {} 字节：", bytes.len());
        for (i, chunk) in bytes.chunks(16).enumerate() {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
            let text: String = chunk
                .iter()
                .map(|&b| {
                    if (0x20..0x7f).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            println!("    {:08X}  {:<47}  {}", i * 16, hex.join(" "), text);
        }
    }

    Ok(())
}
