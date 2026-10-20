//! P0-1 回归：**断点命中之后，链路必须还能用**。
//!
//! 这是 2026-10-20 AI 会话里最致命的那条（`docs/dev/ai-session-issues-2026-10-20.md`）：
//! 断点一命中，`pb_memory_read` 就永久变成 `os error 10053`（对端主动中止），
//! 于是"断点 → 取寄存器/取内存"这个最核心的用法直接废掉。
//!
//! 这里把整条路径在进程内复现：起链路 → 连上 → 下断点 → **真的命中** → 再走一次链路请求。
//! 命中的是我们自己写的函数（`hit_me`），不依赖外部目标，可重复。
#![allow(unsafe_code)]
// 断点靠 **Windows 的向量化异常处理（VEH）** 实现（`breakpoint.rs` 里 `AddVectoredExceptionHandler`），
// 非 Windows 上 `pb_set_breakpoint` 不会真的生效 —— 所以这条回归只在 Windows 上成立，
// 而不是"在 Linux 上也假装跑一遍"。
#![cfg(windows)]

use std::time::{Duration, Instant};

use pinbridge_agent::{breakpoint, exports};
use pinbridge_client::client::{AgentClient, AgentRequest, AgentResponse};
use pinbridge_client::transport::TcpTransport;
use pinbridge_core::capability::Capability;

/// 被下断点的函数：`#[inline(never)]` + `#[no_mangle]` 保证它有独立入口，
/// 且补丁落在这条真实指令上（体积留够，避免被优化成一条 `ret`）。
#[inline(never)]
#[no_mangle]
pub extern "C" fn hit_me(value: u32) -> u32 {
    let mut accumulator = value;
    accumulator = accumulator.wrapping_add(1);
    accumulator = accumulator.wrapping_mul(3);
    accumulator ^ 0x5a5a
}

#[test]
fn link_still_answers_after_a_breakpoint_hit() {
    // ① 起链路（真 agent 导出）
    let dir = std::env::temp_dir().join(format!("pb-bp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let port_file = dir.join("agent-port.txt");
    let addr = exports::start_link_server(&port_file.to_string_lossy()).expect("起链路");

    // agent **自己**那份能力集：下断点要 breakpoint，读内存要 mem_read。
    let caps = Capability::read_only_baseline().with(Capability::Breakpoint);
    assert_eq!(
        exports::pb_set_capabilities(caps.bits()),
        0,
        "下发 agent 能力应当成功"
    );

    // ② 连上（客户端也必须声明能力，否则 agent 侧看到的是 CapabilitySet(0)）
    let transport = TcpTransport::connect(addr).expect("连接链路");
    let mut client = AgentClient::new(transport)
        .with_capabilities(caps)
        .with_timeout(Duration::from_secs(5));
    client.ping().expect("握手应当成功");
    println!("✓ ① 链路已连上 {addr} 并握手");

    // ③ 下断点并**真的命中**
    let target = hit_me as *const () as u64;
    assert_eq!(
        exports::pb_set_breakpoint(target),
        0,
        "下断点应当成功（0x{target:X}）"
    );
    println!("✓ ② 断点已下在 hit_me = 0x{target:X}");

    let before = Instant::now();
    let value = hit_me(7); // 命中 int3 → VEH 抓快照、还原字节 → 继续执行
    println!(
        "✓ ③ 命中并继续：hit_me(7) = {value}（耗时 {:?}）",
        before.elapsed()
    );
    println!(
        "✓ ④ 命中瞬间抓到哪里：{} 条记录",
        breakpoint::take_pending().len()
    );

    // ④ **关键判据**：命中之后再走一次链路请求。
    //    修之前这里是 os error 10053（对端中止）——即服务端在处理命中之后就不能应答了。
    let response = client.call(&AgentRequest::ReadMemory {
        address: target,
        length: 8,
    });
    match response {
        Ok(AgentResponse::Memory { address, hex_bytes }) => {
            println!(
                "✓ ⑤ 命中之后链路仍可用：0x{address:X} 读回 {} 字节 {hex_bytes}",
                hex_bytes.len() / 2
            );
            assert_eq!(hex_bytes.len(), 16, "读回应当是 8 字节 = 16 个十六进制字符");
        }
        other => panic!(
            "✗ 命中之后链路不可用（P0-1 复现）：{other:?}\n\
             这是服务端在命中之后失去应答能力，不是目标侧问题。"
        ),
    }
}
