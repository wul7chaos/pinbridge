//! `pb_link_start`：agent 自己把链路服务起起来，并把端口写进文件。
//!
//! 这个测试文件**必须**能用 `unsafe`：它验的就是 C ABI 导出（`*const c_char` 入参），
//! 而这正是 `pinbridge-client` 库里刻意禁止的东西。放开范围仅限本文件，且每处都写了
//! SAFETY 说明。
#![allow(unsafe_code)]

//!
//! # 这条用例为什么存在
//!
//! `LinkServer` 与 `ClientLink` 早就写好了，但**没有任何生产入口启动它** —— 以前只有测试
//! 在进程内 `LinkServer::bind()`。于是"注入进目标"之后 Hub 根本连不上 agent
//! （见 `docs/dev/attach-capability-gaps.md` 的 G3）。
//!
//! `pb_link_start` 就是那个入口。它唯一能说明问题的验证方式是：
//! **调它 → 文件里出现端口 → 真的连上去握手**。只断言"返回 0"是不够的
//! （那正是"桩函数返回成功"的经典假通过）。

use std::ffi::CString;
use std::time::Duration;

use pinbridge_agent::exports;
use pinbridge_client::client::AgentClient;
use pinbridge_client::transport::TcpTransport;
use pinbridge_core::capability::Capability;

#[test]
fn link_start_binds_a_loopback_port_and_writes_it_to_a_file() {
    let dir = std::env::temp_dir().join(format!("pb-link-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let port_file = dir.join("agent-port.txt");
    let path = CString::new(port_file.to_string_lossy().as_bytes()).expect("路径不该含 NUL");

    // SAFETY: 指针非空、以 NUL 结尾，且在调用期间有效（CString 活到最后）。
    let code = unsafe { exports::pb_link_start(path.as_ptr()) };
    assert_eq!(code, 0, "pb_link_start 应当返回 PB_STATUS_OK(0)");

    let text = std::fs::read_to_string(&port_file).expect("端口文件应当被写出来");
    let addr: std::net::SocketAddr = text
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("端口文件内容应当是 127.0.0.1:PORT，实际是 {text:?}"));
    assert!(addr.ip().is_loopback(), "链路只能绑回环，实际 {addr}");
    println!("✓ pb_link_start 把端口写进了 {port_file:?}：{addr}");

    // 幂等：再调一次必须给**同一个**端口（否则每次附加都换端口，调用方没法稳定连上）。
    let again = unsafe { exports::pb_link_start(path.as_ptr()) };
    assert_eq!(again, 0, "重复调用也应当成功");
    let text2 = std::fs::read_to_string(&port_file).expect("端口文件应当被重写");
    assert_eq!(
        text.trim(),
        text2.trim(),
        "重复调用应当返回同一个端口：第一次 {text:?}，第二次 {text2:?}"
    );
    println!("✓ 幂等：重复调用仍是 {addr}");

    // 真的连得上并握手 —— 这才是"Hub 够得到 agent"的证据。
    let transport = TcpTransport::connect(addr).expect("连接刚起的链路");
    let mut client = AgentClient::new(transport)
        .with_capabilities(Capability::read_only_baseline())
        .with_timeout(Duration::from_secs(5));
    let info = client.ping().expect("握手应当成功");
    assert!(
        info.version.contains("pinbridge-agent"),
        "版本串应当说明对端是谁：{}",
        info.version
    );
    println!("✓ 连上并握手：{} / {}", info.version, info.arch);
}

#[test]
fn link_start_rejects_a_null_path() {
    // 契约：`*const c_char` 允许为 NULL，此时返回 PB_STATUS_INVALID_ARGUMENT(-1)。
    // SAFETY: 传 NULL 是被允许的（这条用例就是在验它不会崩）。
    let code = unsafe { exports::pb_link_start(std::ptr::null()) };
    assert_eq!(
        code, -1,
        "NULL 路径应当返回 PB_STATUS_INVALID_ARGUMENT(-1)，实际 {code}"
    );
}
