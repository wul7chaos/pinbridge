//! `pinbridge-mcp` 可执行入口：在 stdio 上跑 MCP 服务。
//!
//! 约定：**stdout 只允许出现 JSON-RPC 消息**。任何调试输出必须走 stderr，
//! 否则 AI 客户端会在自己的一帧里收到半行日志并直接解析失败——
//! 这类问题排查起来非常费时，所以从第一版就把它当硬约束。

use std::process::ExitCode;

use pinbridge_hub::HubService;
use pinbridge_mcp::{serve_stdio, McpServer};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trace_dir = args
        .iter()
        .position(|a| a == "--trace-dir")
        .and_then(|i| args.get(i + 1))
        .map_or_else(
            || std::env::temp_dir().join("pinbridge-traces"),
            std::path::PathBuf::from,
        );

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "pinbridge-mcp —— MCP stdio 服务\n\n\
用法: pinbridge-mcp [--trace-dir <DIR>]\n\n\
  --trace-dir <DIR>  轨迹输出目录（默认系统临时目录下的 pinbridge-traces）\n\n\
本进程通过 stdio 与 AI 客户端通信，stdout 只输出 JSON-RPC。"
        );
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "pinbridge-mcp {} 启动（轨迹目录: {}）",
        env!("CARGO_PKG_VERSION"),
        trace_dir.display()
    );
    let mut server = McpServer::new(HubService::with_trace_dir(trace_dir));
    let stdin = std::io::stdin();
    match serve_stdio(&mut server, stdin.lock(), std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("MCP 服务异常退出: {e}");
            ExitCode::FAILURE
        }
    }
}
