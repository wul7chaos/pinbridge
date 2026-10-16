//! `pinbridge` 命令行。
//!
//! 定位：**在没有 AI 客户端、没有 Pin、没有目标程序的情况下也能跑起来的东西**。
//! 具体提供三件事：
//!
//! 1. `pb tools` —— 打印 MCP 工具表（人类校对工具描述的第一入口）；
//! 2. `pb replay <file.pbt>` —— 离线读轨迹并给出汇总（`docs/design/offline-replay.md` 的第一步）；
//! 3. `pb demo` —— 端到端自检：建会话 → 授权 → 装脚本 → 录轨迹 → 折叠查询 → 读回校验。
//!
//! 第 3 条是刻意做进命令行的：一个框架的"骨架是否真的通了"必须能在一条命令里
//! 被验证。CI 里跑 `pb demo` 比跑一百个单元测试更能说明"端到端还能工作"。

use std::collections::BTreeSet;
use std::process::ExitCode;

use pinbridge_core::capability::Capability;
use pinbridge_core::principal::Principal;
use pinbridge_core::session::TargetSpec;
use pinbridge_hub::HubService;
use pinbridge_proto::frames::ControlCommand;
use pinbridge_proto::mcp::TOOLS;
use pinbridge_trace::callgraph::{build as build_call_graph, CallGraphOptions};
use pinbridge_trace::diff::{attach_warnings, diff_traces, diff_warnings, DiffOptions};
use pinbridge_trace::event::EventKind;
use pinbridge_trace::fold::expand_records;
use pinbridge_trace::pbt::{PbtReader, Summary};

/// 报告里最多列出几处分岔（总数仍然准确）。
const MAX_DIFFERENCES: usize = 8;

const USAGE: &str = "\
pinbridge 0.1.0 —— 插装式动态分析框架（AI 主导 · 人机平级）

用法:
  pinbridge tools                     打印 MCP 工具表
  pinbridge replay <FILE.pbt> [--head N]
                                      离线读取轨迹并打印汇总/前 N 条记录
  pinbridge diff <A.pbt> <B.pbt> [--ignore-address]
                                      同一个目标的两次运行：行为在哪里分岔
  pinbridge calls <FILE.pbt> [--top N] [--strict-code-targets]
                                      从转移事件重建调用关系（需要录制时开 -pb_transfers）
  pinbridge demo [--trace-dir <DIR>]  端到端自检
  pinbridge mcp                       以 stdio 方式启动 MCP 服务
  pinbridge version                   打印版本

说明:
  * 本命令行在 M0 阶段即可离线运行（不需要 Pin、不需要目标程序）；
  * 需要 Pin 的动作会显式返回 not_supported，而不是静默成功。
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "tools" => {
            print_tools();
            ExitCode::SUCCESS
        }
        "version" => {
            println!("pinbridge {} (abi {})", env!("CARGO_PKG_VERSION"), 1);
            ExitCode::SUCCESS
        }
        "replay" => match args.get(1) {
            Some(path) => {
                let head = parse_head(&args).unwrap_or(10);
                match replay(path, head) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("重放失败: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            None => {
                eprintln!("用法: pinbridge replay <FILE.pbt> [--head N]");
                ExitCode::FAILURE
            }
        },
        "diff" => match (args.get(1), args.get(2)) {
            (Some(left), Some(right)) => {
                let options = DiffOptions {
                    ignore_address: args.iter().any(|a| a == "--ignore-address"),
                    ignore_thread: args.iter().any(|a| a == "--ignore-thread"),
                };
                match diff(left, right, options) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("差分失败: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!(
                    "用法: pinbridge diff <A.pbt> <B.pbt> [--ignore-address] [--ignore-thread]"
                );
                ExitCode::FAILURE
            }
        },
        "calls" => match args.get(1) {
            Some(path) => {
                let top = parse_top(&args).unwrap_or(10);
                match calls(path, top) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("建图失败: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            None => {
                eprintln!("用法: pinbridge calls <FILE.pbt> [--top N]");
                ExitCode::FAILURE
            }
        },
        "demo" => {
            let dir = parse_trace_dir(&args);
            match demo(&dir) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("自检失败: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "mcp" => {
            let dir = parse_trace_dir(&args);
            let mut server = pinbridge_mcp::McpServer::new(HubService::with_trace_dir(dir));
            let stdin = std::io::stdin();
            match pinbridge_mcp::serve_stdio(&mut server, stdin.lock(), std::io::stdout().lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("MCP 服务异常退出: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("未知子命令: {other}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn print_tools() {
    println!("MCP 工具表（共 {} 个）\n", TOOLS.len());
    for tool in TOOLS {
        let requires: Vec<&str> = tool.requires.iter().map(|c| c.name()).collect();
        let requires = if requires.is_empty() {
            "—".to_owned()
        } else {
            requires.join("|")
        };
        println!("  {}", tool.name);
        println!("    需要能力 : {requires}");
        println!(
            "    目标内执行: {}",
            if tool.executes_in_target {
                "是（会在审计里留痕）"
            } else {
                "否"
            }
        );
        println!("    说明     : {}", tool.description);
        println!();
    }
}

fn parse_head(args: &[String]) -> Option<usize> {
    let index = args.iter().position(|a| a == "--head")?;
    args.get(index + 1)?.parse().ok()
}

fn parse_top(args: &[String]) -> Option<usize> {
    let index = args.iter().position(|a| a == "--top")?;
    args.get(index + 1)?.parse().ok()
}

fn parse_trace_dir(args: &[String]) -> std::path::PathBuf {
    let index = args.iter().position(|a| a == "--trace-dir");
    index.and_then(|i| args.get(i + 1)).map_or_else(
        || std::env::temp_dir().join("pinbridge-traces"),
        std::path::PathBuf::from,
    )
}

/// 读一份轨迹并展开折叠，返回（展开后的事件、汇总）。
///
/// 两个刻意的选择：
///
/// 1. **用流式模式读**（容忍被截断的尾部）。严格模式在"录制被强杀/目标崩溃"时
///    直接报损坏、读不出任何东西——而那种轨迹恰恰是最想拿来比对的
///    （"这次为什么崩了"）。容忍截断 + 把"汇总缺失/不完整"变成**告警**，
///    比"读不出来"有用得多；
/// 2. **展开折叠后比对**。折叠对时序敏感，两次运行的折叠位置不同会制造大量
///    伪分岔（见 `pinbridge_trace::diff` 的告警）。
fn load_trace(path: &str) -> Result<(Vec<pinbridge_trace::Event>, Option<Summary>), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut reader = PbtReader::open_streaming(file).map_err(|e| format!("{path}: {e}"))?;
    let records = reader.read_all().map_err(|e| format!("{path}: {e}"))?;
    let summary = reader.summary().map_err(|e| format!("{path}: {e}"))?;
    Ok((expand_records(&records), summary))
}

/// `pinbridge diff A.pbt B.pbt`：两次运行的行为分岔在哪里。
fn diff(left_path: &str, right_path: &str, options: DiffOptions) -> Result<(), String> {
    let (left, left_summary) = load_trace(left_path)?;
    let (right, right_summary) = load_trace(right_path)?;
    let mut result = diff_traces(&left, &right, options, MAX_DIFFERENCES);
    attach_warnings(
        &mut result,
        diff_warnings(left_summary.as_ref(), right_summary.as_ref()),
    );
    println!("左（基准）: {left_path}");
    println!("右（对照）: {right_path}");
    // 说清本命令**已经**做了什么：上面那条折叠告警会让读者以为"还得先展开"，
    // 而 CLI 在读取时就已经展开了（否则会让人去做一件已经做过的事）。
    println!("（读取时已自动展开折叠，按展开后的事件逐条比对）");
    print!("{}", result.render());
    // 有告警时**额外**在最后再提一次：报告很长的时候，第一行的告警会被滚出屏幕，
    // 而"这份数据能不能用来下结论"是唯一不能漏的信息。
    if result.has_warnings() {
        println!(
            "⚠ 上面有 {} 条告警：结论的可信度受限，先处理告警再看分岔。",
            result.warnings.len()
        );
    }
    Ok(())
}

/// `pinbridge calls A.pbt`：从转移事件重建调用关系（规则见 ADR 0012）。
fn calls(path: &str, top: usize) -> Result<(), String> {
    let (events, _summary) = load_trace(path)?;
    let graph = build_call_graph(
        &events,
        CallGraphOptions {
            strict_code_targets: std::env::args().any(|a| a == "--strict-code-targets"),
        },
    );
    println!("文件: {path}（已按展开后的事件建图）");
    print!("{}", graph.render(top));
    Ok(())
}
fn replay(path: &str, head: usize) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut reader = PbtReader::open(file).map_err(|e| e.to_string())?;
    let records = reader.read_all().map_err(|e| e.to_string())?;
    let summary = reader.summary().map_err(|e| e.to_string())?;
    let expanded = expand_records(&records);

    println!("文件       : {path}");
    println!(
        "版本/标志  : v{} flags=0x{:02X}",
        reader.version(),
        reader.flags()
    );
    if let Some(meta) = non_empty(reader.meta()) {
        println!("元数据     : {meta}");
    }
    match &summary {
        Some(s) => {
            println!(
                "汇总       : records={} events={} folded={} dropped={} truncated={}",
                s.records, s.events, s.repeat_folded, s.dropped, s.truncated
            );
            println!("折叠率     : {:.4}", s.fold_ratio());
        }
        None => println!("汇总       : 缺失（轨迹可能被中断，尾部信息不完整）"),
    }
    println!(
        "记录/展开  : {} 条记录 → {} 条事件",
        records.len(),
        expanded.len()
    );

    // 事件类型分布：判断"这段轨迹里主要发生了什么"的第一眼信息
    let mut kinds: BTreeSet<&str> = BTreeSet::new();
    for record in &records {
        kinds.insert(record.kind.name());
    }
    println!(
        "类型分布   : {}",
        kinds.into_iter().collect::<Vec<_>>().join(", ")
    );

    if head > 0 {
        println!("\n前 {head} 条记录：");
        for record in records.iter().take(head) {
            println!("  {}", record.to_text());
        }
    }
    let audit = non_empty(reader.audit_log());
    if let Some(audit) = audit {
        println!("\n审计（前 5 行）：");
        for line in audit.lines().take(5) {
            println!("  {line}");
        }
    }
    Ok(())
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// 端到端自检。每一步都打印发生了什么，最后校验"录下来的轨迹能否还原现场"。
fn demo(trace_dir: &std::path::Path) -> Result<(), String> {
    let human = Principal::human("operator");
    let ai = Principal::ai("demo-agent");
    let mut hub = HubService::with_trace_dir(trace_dir);

    println!("== 1. 人类建立会话 ==");
    let session = hub
        .create_session(&human, TargetSpec::launch("TARGET.exe", "x86_64"))
        .map_err(|e| e.to_string())?;
    hub.attach(&human, session, 4242, "TARGET.exe --demo")
        .map_err(|e| e.to_string())?;
    let epoch = hub.epoch_of(session).map_err(|e| e.to_string())?;
    println!("   session={session} pid=4242 epoch={epoch}");

    println!("== 2. AI 以只读身份尝试下断点（应当被拒）==");
    match hub.control(
        &ai,
        session,
        epoch,
        &ControlCommand::BreakpointSet {
            address: 0x1400_01000,
            label: "oep".into(),
        },
    ) {
        Ok(()) => return Err("只读主体不应能下断点".into()),
        Err(e) => println!("   已拒绝: {e}"),
    }

    println!("== 3. 人类授权（能力只能不超过会话上限）==");
    let granted = hub
        .grant_capabilities(&human, session, epoch, &ai, &Capability::ALL)
        .map_err(|e| e.to_string())?;
    let names: Vec<&str> = granted.iter().map(Capability::name).collect();
    println!("   已授予: {}", names.join("|"));
    println!("   （能力集位图 {}）", granted.bits());

    println!("== 4. 下断点 + 加载脚本 ==");
    hub.control(
        &human,
        session,
        epoch,
        &ControlCommand::BreakpointSet {
            address: 0x1400_01000,
            label: "oep".into(),
        },
    )
    .map_err(|e| e.to_string())?;
    let script = hub
        .load_script(
            &ai,
            session,
            epoch,
            "unpack_hook.py",
            SAMPLE_SCRIPT,
            vec![EventKind::SyscallEnter, EventKind::MemWrite],
        )
        .map_err(|e| e.to_string())?;
    println!("   script={script} sha256={}", hash_prefix(&hub, session));

    println!("== 5. 录制轨迹并灌入模拟事件 ==");
    let path = hub
        .start_trace(&human, session, epoch, "repeat+delta")
        .map_err(|e| e.to_string())?;
    println!("   轨迹: {path}");
    let total = push_synthetic_trace(&mut hub, session)?;
    let summary = hub
        .stop_trace(&human, session, epoch)
        .map_err(|e| e.to_string())?;
    println!(
        "   原始 {total} 条 → 记录 {} 条（折叠 {} 条，折叠率 {:.3}）",
        summary.records,
        summary.repeat_folded,
        summary.fold_ratio()
    );
    if summary.dropped > 0 {
        println!(
            "   警告: {} 条事件因背压被丢弃，轨迹不完整",
            summary.dropped
        );
    }

    println!("== 6. 折叠查询（AI 的观察视角）==");
    let (events, next, dropped) = hub
        .query_events(&ai, session, 1, 8, true)
        .map_err(|e| e.to_string())?;
    for view in &events {
        println!(
            "   #{:<5} {:<14} tid={} addr=0x{:X} repeats={}",
            view.seq.get(),
            view.kind,
            view.thread,
            view.address,
            view.repeats
        );
    }
    println!("   next={next} dropped={dropped}");

    println!("== 7. 读回轨迹并校验可逆性 ==");
    let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let mut reader = PbtReader::open(file).map_err(|e| e.to_string())?;
    let records = reader.read_all().map_err(|e| e.to_string())?;
    let expanded = expand_records(&records);
    if expanded.len() != total {
        return Err(format!("轨迹还原失败: {} != {total}", expanded.len()));
    }
    if let Some(meta) = non_empty(reader.meta()) {
        println!("   meta: {meta}");
    }
    println!("   OK: {} 条记录无损还原为 {total} 条事件", records.len());

    println!("== 8. 操作溯源 ==");
    for line in hub.provenance_of(session).iter().take(6) {
        println!("   {line}");
    }
    println!("\n自检通过。轨迹目录: {}", trace_dir.display());
    Ok(())
}

fn hash_prefix(hub: &HubService, session: pinbridge_core::id::SessionId) -> String {
    hub.scripts(session).and_then(|all| all.last()).map_or_else(
        || "—".to_owned(),
        |s| s.source_sha256.chars().take(12).collect(),
    )
}

/// 灌入一段"像 VMP 外壳"的合成事件流：
/// 一个 4000 次的解密循环（同址同参 → 应被折叠）+ 若干系统调用与内存写入。
fn push_synthetic_trace(
    hub: &mut HubService,
    session: pinbridge_core::id::SessionId,
) -> Result<usize, String> {
    let mut total = 0usize;
    for _ in 0..4000 {
        hub.push_event(
            session,
            EventKind::SyscallEnter,
            100,
            0x7FF6_0000_2000,
            &[(0, 0x40), (1, 0x7FF6_0000_2300)],
        )
        .map_err(|e| e.to_string())?;
        total += 1;
    }
    for i in 0..64u64 {
        hub.push_event(
            session,
            EventKind::MemWrite,
            100,
            0x7FF6_0000_3000 + i * 8,
            &[(0, 0x7FF6_0000_3000 + i * 8), (1, 8), (2, 0xDEAD_0000 + i)],
        )
        .map_err(|e| e.to_string())?;
        total += 1;
    }
    for call in [0x1000u64, 0x1010, 0x1030, 0x1F00] {
        hub.push_event(
            session,
            EventKind::SyscallExit,
            100,
            0x7FF6_0000_2000,
            &[(0, call), (1, 0)],
        )
        .map_err(|e| e.to_string())?;
        total += 1;
    }
    hub.push_event(
        session,
        EventKind::BreakpointHit,
        100,
        0x1400_01000,
        &[(0, 0x1400_01000)],
    )
    .map_err(|e| e.to_string())?;
    total += 1;
    Ok(total)
}

/// 演示脚本。真实场景里这段代码会跑在目标进程的 CPython 里。
const SAMPLE_SCRIPT: &str = r#"# unpack_hook.py —— 脱壳时机判定（示意）
import pb

_OEP_CANDIDATES = []

def on_syscall(ev):
    # 观测到 VirtualProtect 改属性后，标记为潜在 OEP
    if ev.args[0] == 0x50 and ev.args[1] & 0x20:
        _OEP_CANDIDATES.append(ev.address)
        pb.log("potential oep at 0x%x" % ev.address)

def on_mem_write(ev):
    # 自解压完成后 dump
    if ev.address >= 0x7FF6_0000_3000:
        pb.snapshot(ev.address, 0x1000)

pb.subscribe("syscall_enter", on_syscall)
pb.subscribe("mem_write", on_mem_write)
"#;
