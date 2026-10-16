//! Hub 侧客户端 ↔ 真实回环链路 ↔ 真实 agent 的端到端测试。
//!
//! 这条测试覆盖的是 `docs/roadmap.md` M1 里的
//! 「Hub ↔ agent 回环链路（真实握手、版本协商、重连）」：
//!
//! ```text
//!   AgentClient（Hub 侧）── TcpTransport（127.0.0.1）──▶ LinkServer（agent 侧）
//!                                                            └─▶ exports::pb_*
//! ```
//!
//! 它验证四件在单元测试里**测不到**的事：
//!
//! 1. 真握手：`Ping` 走真实 socket，拿回协议版本、架构与能力集；
//! 2. 版本协商：不匹配的 `wire_version` 被明确拒绝，而不是"能解析就继续"；
//! 3. **两道闸门都生效且可区分**：Hub 侧能力不足在发送前就被拦（报文根本没出门），
//!    目标侧能力不足由 agent 拒绝（证明"绕过客户端也拿不到写权限"）；
//! 4. 事件真的能过链路：agent 推事件 → Hub 拉取 → 序号/类型/参数完好。

use std::net::SocketAddr;
use std::time::Duration;

use pinbridge_agent::exports;
use pinbridge_agent::host::AgentHost;
use pinbridge_agent::{AgentState, LinkServer};
use pinbridge_client::client::{AgentClient, AgentRequest, AgentResponse, ClientError};
use pinbridge_client::transport::TcpTransport;
use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_core::error::Error;
use pinbridge_trace::event::{Event, EventKind};

/// 起一个只服务一条连接的服务端线程，返回它的地址与 join 句柄。
fn spawn_server() -> (SocketAddr, std::thread::JoinHandle<usize>) {
    let server = LinkServer::bind().expect("绑定回环端口");
    let addr = server.addr();
    assert!(addr.ip().is_loopback(), "链路只能绑回环地址");
    let handle = std::thread::spawn(move || server.serve_connections(1).expect("服务端不应报错"));
    (addr, handle)
}

fn reset_agent(capabilities: CapabilitySet) {
    exports::pb_detach();
    exports::pb_set_capabilities(capabilities.bits());
}

#[test]
fn hub_client_drives_a_real_agent_over_a_real_socket() {
    // 这里**不需要** GLOBAL_STATE_LOCK：集成测试是独立的测试二进制（独立进程），
    // 与库自身的单元测试不共享全局状态。只要这个文件里只有一个用例
    // （或所有用例都不并发跑），就不会有交叉 —— 而服务端线程与主线程
    // 恰恰**应当**并发访问同一份状态，这正是生产里的形态。
    let baseline = Capability::read_only_baseline();
    reset_agent(baseline);

    let (addr, server) = spawn_server();
    let transport = TcpTransport::connect(addr).expect("连接回环端口");
    assert!(transport.peer().ip().is_loopback());

    let mut client = AgentClient::new(transport)
        .with_capabilities(baseline)
        .with_timeout(Duration::from_secs(5));

    // ── 1. 真握手 ────────────────────────────────────────────────────────
    let info = client.ping().expect("握手应当成功");
    assert!(
        info.version.contains("pinbridge-agent"),
        "版本串应当说明对端是谁: {}",
        info.version
    );
    assert!(!info.arch.is_empty(), "架构不能为空");
    assert_eq!(
        info.capabilities, baseline,
        "agent 回报的能力集必须等于下发过的（只读基线 257）"
    );
    println!(
        "✓ 握手成功：{} / {} / {:?}",
        info.version, info.arch, info.capabilities
    );

    // ── 2. 第一道闸门：**客户端侧**能力不足，报文不该出门 ────────────────
    let error = client
        .call(&AgentRequest::WriteMemory {
            address: 0x1000,
            hex_bytes: "90".into(),
        })
        .expect_err("只读客户端不允许写内存");
    match error {
        ClientError::Core(Error::CapabilityDenied { needed, held }) => {
            assert_eq!(needed, Capability::MemWrite);
            assert_eq!(held, baseline);
            println!("✓ 客户端侧拦截：needed={needed:?} held={held:?}");
        }
        other => panic!("期望客户端侧能力不足，实际: {other}"),
    }

    // ── 3. 第二道闸门：**目标侧**能力不足（绕过客户端也拿不到写权限）────
    // 把**客户端**的能力放开，但目标侧仍保持只读：此时请求会真的出门，
    // 由 agent 拒绝。这正是"双端校验"要防的那件事——客户端校验只是省事，
    // 真正的边界在目标进程里。
    let mut client = client.with_capabilities(Capability::operator_default());
    let error = client
        .call(&AgentRequest::WriteMemory {
            address: 0x1000,
            hex_bytes: "90".into(),
        })
        .expect_err("目标侧只读，写内存必须被拒绝");
    match error {
        ClientError::Rejected { code, message } => {
            assert_eq!(code, "capability_denied", "错误码必须机器可读: {message}");
            println!("✓ 目标侧拒绝：{code} / {message}");
        }
        other => panic!("期望 agent 拒绝，实际: {other}"),
    }

    // ── 4. 版本协商：不匹配必须明确拒绝 ──────────────────────────────────
    let error = client
        .call(&AgentRequest::Ping { wire_version: 999 })
        .expect_err("协议版本不匹配必须被拒");
    match error {
        ClientError::Rejected { code, message } => {
            assert_eq!(code, "protocol_error");
            assert!(message.contains("999"), "错误文本要给出双方版本: {message}");
            println!("✓ 版本协商生效：{message}");
        }
        other => panic!("期望协议错误，实际: {other}"),
    }

    // ── 5. 事件真的过链路 ────────────────────────────────────────────────
    // 模拟探针侧：直接往 agent 里推事件（真实场景里这是 Pin 探针干的活）。
    for index in 0..3u64 {
        let sequence = exports::pb_allocate_sequence();
        let event = Event {
            sequence,
            kind: EventKind::SyscallEnter,
            thread_id: 100,
            address: 0x7FF6_0000_2000 + index * 8,
            args: [0x40 + index, 0x7FF6_0000_2300, 0, 0, 0, 0, 0, 0],
        };
        AgentHost::with(|state: &mut AgentState| state.push_event(event)).expect("事件入队");
    }
    match client
        .call(&AgentRequest::DrainEvents {
            from: 1,
            limit: 8,
            fold_repeats: false,
        })
        .expect("拉取事件")
    {
        AgentResponse::Events {
            items,
            next,
            dropped,
        } => {
            assert_eq!(items.len(), 3, "推入 3 条应当取回 3 条");
            assert_eq!(dropped, 0, "没有背压就不该报丢弃");
            assert_eq!(next, items[2].seq + 1, "游标指向下一条");
            assert_eq!(items[0].kind, EventKind::SyscallEnter.code());
            assert_eq!(items[0].thread, 100);
            assert_eq!(
                items[0].args,
                vec![0x40, 0x7FF6_0000_2300],
                "参数跨链路完好"
            );
            println!("✓ 事件往返：{:?}", items[0]);
        }
        other => panic!("期望事件批，实际: {other:?}"),
    }

    // ── 6. 第二道闸门的另一半：能力够了之后，桩必须明确说"不支持" ────────
    // 先把**目标侧**的能力放开（Hub 侧早已放开）。这时 Pause 会过能力那关、
    // 落到 M0 的桩上——两道闸门必须给出**可区分**的错误码，否则使用者
    // 无法判断"我该去要权限"还是"这个功能还没做"。
    reset_agent(Capability::operator_default());
    // 用**单步**当"有权限但未实现"的例子：暂停/继续已经是真实实现（采集语义），
    // 而单步明确不做调试器语义（见 `pb_single_step` 的文档）。
    let error = client
        .call(&AgentRequest::SingleStep { count: 1 })
        .expect_err("单步仍未实现");
    match error {
        ClientError::Rejected { code, message } => {
            assert_eq!(
                code, "not_supported",
                "未实现必须报 not_supported: {message}"
            );
            assert!(
                message.contains("M0")
                    || message.contains("未实现")
                    || message.contains("Pin")
                    || message.contains("M1"),
                "不支持的说明要指出后续怎么办: {message}"
            );
            println!("✓ 未实现的能力明确失败：{message}");
        }
        other => panic!("期望 not_supported，实际: {other}"),
    }

    // 暂停/继续现在是真实现：过能力闸门之后必须**成功**，并且状态可查。
    client
        .call(&AgentRequest::Pause)
        .expect("有权限时 pause 应当成功");
    println!("✓ 目标侧 pause 现在成真（采集语义）");

    drop(client);
    let handled = server.join().expect("服务端线程");
    // 精确计数而不是 `>=`：被客户端拦下的那条写请求**根本没有出门**，
    // 所以服务端应当恰好处理 5 条（ping / 被目标拒绝的写 / 版本不匹配的 ping /
    // drain / pause）。这条断言顺带证明了"客户端侧校验发生在发送之前"。
    // ping / 被目标拒绝的写 / 版本不匹配的 ping / drain / 单步（未实现）/ 暂停
    assert_eq!(
        handled, 6,
        "服务端应当处理 5 条请求（客户端拦下的那条不该到达）"
    );
    println!("✓ 服务端共处理 {handled} 条请求");
}
