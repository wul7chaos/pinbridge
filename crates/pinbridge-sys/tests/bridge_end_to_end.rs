//! Hub 侧加载器的端到端测试：**真实加载一个 DLL 并驱动它**。
//!
//! 这条测试对应 docs/roadmap.md 的 M1 验收标准：
//! 「`pinbridge-sys` 加载器端到端 —— `Api::load` 对一个真实构建出的 DLL
//! 成功并校验 ABI」。
//!
//! 为什么它比其它测试更重要：`pinbridge-sys` 是 Hub 与目标端之间唯一的桥，
//! 而在此之前它只被"缺少 DLL 时给出可定位错误"这一类单元测试覆盖过。
//! 真实的 `LoadLibraryA` → `GetProcAddress` → 函数指针调用 → 状态码翻译 ——
//! 这条路径上任何一处错位都会表现为"目标进程崩了"，而不是"测试失败"。
//!
//! 被测对象是 `native/` 构建出的 `pinbridge_bridge.dll`（不需要 Intel Pin）：
//! 它导出 Hub 期望的 8 个 tool 符号，内部再把调用转发给真实的 Rust agent DLL。
//!
//! ## 找不到 DLL 时
//!
//! 默认**跳过**（多数贡献者机器上没有 C++ 工具链），但设置
//! `PINBRIDGE_REQUIRE_NATIVE=1` 时会改为失败。CI 必须设这个变量——
//! 否则这条测试会因为"环境没准备好"而永久静默跳过，
//! 那等于用一条绿色的测试掩盖"从来没跑过"。
//!
//! 需要一个位置合适的 agent DLL：本测试会把 `target/<profile>/pinbridge_agent.dll`
//! 拷到桥接 DLL 同目录，顺便验证桥接层的"同目录查找"这条真实加载路径。

// 需要调用 `last_error_string()`（unsafe：DLL 内的指针在下一次错误写入前有效）。
// 本文件就是 ABI 边界的测试，unsafe 是它存在的理由。
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use pinbridge_core::capability::Capability;
use pinbridge_core::error::Error;
use pinbridge_sys::abi::PbSet;

/// 找到桥接 DLL：优先环境变量，其次仓库内的构建产物目录。
fn locate_bridge() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PINBRIDGE_BRIDGE_DLL") {
        if !explicit.is_empty() {
            let path = PathBuf::from(&explicit);
            return path.is_file().then_some(path);
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    [
        root.join("native/build/pinbridge_bridge.dll"),
        root.join("native/build/Release/pinbridge_bridge.dll"),
        root.join("native/build/dist/pinbridge_bridge.dll"),
        // 带 Pin 的构建目录（scripts/build-native.ps1 -WithPin）：
        // 桥接 DLL 与 agent DLL 都在这里，两个都有就不必再拷。
        root.join("native/build-pin/pinbridge_bridge.dll"),
        root.join("native/build-pin/dist/pinbridge_bridge.dll"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// agent DLL 由 cargo 构建（`cargo build -p pinbridge-agent`）。
fn locate_agent() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["debug", "release"] {
        let candidate = root.join(format!("target/{profile}/pinbridge_agent.dll"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn skip_or_fail(reason: &str) {
    // CI 会设置 PINBRIDGE_REQUIRE_NATIVE：那样"环境没准备好"必须是一次失败，
    // 而不是一条年年绿色的跳过记录。用 assert! 而不是 if + panic!，
    // 是为了让失败信息在测试报告里保持"前置条件不满足"的语义。
    assert!(
        std::env::var("PINBRIDGE_REQUIRE_NATIVE").is_err(),
        "PINBRIDGE_REQUIRE_NATIVE 已设置，但测试前提不满足：{reason}\n\
         请先执行：\n\
         \x20 cargo build -p pinbridge-agent\n\
         \x20 cmake -S native -B native/build -G Ninja -DCMAKE_BUILD_TYPE=Release\n\
         \x20 cmake --build native/build"
    );
    println!("跳过：{reason}");
}

#[test]
fn hub_side_loader_drives_a_real_dll() {
    if !cfg!(windows) {
        println!("跳过：M0 的加载器只在 Windows 上实现（其它平台返回明确的“不支持”）");
        return;
    }

    let Some(bridge) = locate_bridge() else {
        skip_or_fail("找不到 pinbridge_bridge.dll（native/ 尚未构建）");
        return;
    };
    let Some(agent) = locate_agent() else {
        skip_or_fail("找不到 pinbridge_agent.dll（先 cargo build -p pinbridge-agent）");
        return;
    };

    // 把 agent 放到桥接 DLL 同目录：这既是"让它能被找到"的手段，
    // 也是在验证桥接层的**同目录查找**这条真实路径（而不是靠环境变量作弊）。
    let sibling = bridge.with_file_name("pinbridge_agent.dll");
    if sibling != agent {
        std::fs::copy(&agent, &sibling)
            .unwrap_or_else(|error| panic!("无法把 agent DLL 拷到 {}：{error}", sibling.display()));
    }
    println!("桥接 DLL: {}", bridge.display());
    println!("agent DLL: {}", sibling.display());

    // ── 1. 加载 + ABI 版本 ────────────────────────────────────────────────
    let api = pinbridge_sys::load(bridge.to_str().expect("路径应为有效 UTF-8"))
        .expect("加载真实 DLL 必须成功");
    assert_eq!(
        api.abi_version(),
        pinbridge_sys::abi::PB_ABI_VERSION,
        "DLL 自报的 ABI 版本必须与本构建一致（否则加载器应拒绝加载）"
    );
    println!("✓ 加载成功，ABI 版本 {} 一致", api.abi_version());

    // ── 2. 参数校验先于状态判断 ───────────────────────────────────────────
    let mut empty: Vec<pinbridge_sys::abi::PbEventRaw> = Vec::new();
    let error = api
        .drain(&mut empty)
        .expect_err("容量为 0 必须被拒（不允许“传 0 就当查询条数”的隐式语义）");
    assert!(
        matches!(error, Error::Protocol(_)),
        "参数非法应映射为 Protocol，实际: {error:?}"
    );
    println!("✓ 容量为 0 被拒：{error}");

    // ── 3. 正常取回空队列 ────────────────────────────────────────────────
    let mut buffer = [pinbridge_sys::abi::PbEventRaw::EMPTY; 4];
    let count = api.drain(&mut buffer).expect("空队列的 drain 应当成功");
    assert_eq!(count, 0, "没推过事件时应当取回 0 条");
    println!("✓ drain 正常路径可用（取回 {count} 条）");

    // ── 4. 附加 + 能力下发 ───────────────────────────────────────────────
    api.attach(4242).expect("attach 经桥接层转发应当成功");
    println!("✓ attach 成功");

    let baseline = Capability::read_only_baseline();
    api.apply_capabilities(PbSet::from(baseline))
        .expect("下发只读基线应当成功");

    // ── 5. 跨语言权限链路：Hub 侧看不到 Rust 侧代码，但两边的判断必须一致 ──
    let error = api
        .pause()
        .expect_err("只读基线（mem_read|trace_record）不允许 pause");
    match error {
        Error::CapabilityDenied { needed, held } => {
            assert_eq!(needed, Capability::ProcessControl, "缺的能力必须是进程控制");
            assert_eq!(held, baseline, "持有的能力集必须原样回报（这里是 257）");
            println!("✓ 只读基线被拒（needed={needed:?}，held={held:?}）");
        }
        other => panic!("只读基线 pause 应当报能力不足，实际: {other:?}"),
    }

    // ── 6. 能力足够之后：桩必须明确说"不支持"，而不是转而成功 ────────────
    api.apply_capabilities(PbSet(0xFFFF_FFFF))
        .expect("下发全量能力位应当成功");
    let error = api
        .pause()
        .expect_err("M0 的 pause 是桩，必须明确失败而不是假装成功");
    assert!(
        matches!(error, Error::TargetUnavailable(_)),
        "“不支持”必须映射成 TargetUnavailable（不是 Trace，否则排查方向会跑偏），实际: {error:?}"
    );
    println!("✓ 有权限但实现为桩：{error}");

    // ── 7. 桥接层的能力边界：没有 Pin 就不能启动目标 ──────────────────────
    let error = api
        .start("TARGET.exe", "")
        .expect_err("没有 Pin 的桥接层不能启动目标");
    let text = error.to_string();
    assert!(
        text.contains("Pin"),
        "“不支持”的原因里必须写明需要 Pin，实际: {text}"
    );
    println!("✓ pb_start 明确返回不支持：{text}");

    // ── 8. 错误文本可以跨边界取回 ────────────────────────────────────────
    // SAFETY: api 持有已加载的 DLL，且返回的指针在本次调用后立刻被拷贝成 String
    let last = unsafe { api.last_error_string() }.unwrap_or_default();
    assert!(!last.is_empty(), "失败之后 pb_last_error 必须有内容");
    println!(
        "✓ pb_last_error 可读：{}",
        last.chars().take(60).collect::<String>()
    );

    // ── 9. 目标内录制：Hub 侧驱动 DLL 写轨迹 ──────────────────────────────
    // 这条路径的意义：轨迹由**目标那一侧**写盘，而不是把事件全量搬回 Hub 再拼。
    // 少了 `pb_start_trace`/`pb_stop_trace` 两个符号，"录一份轨迹"就只剩
    // "Hub 自己收事件"这一条路。
    //
    // 参数校验先于状态判断（与上面 `drain` 的 0 容量同一条原则）：
    let error = api
        .start_trace("轨迹\0路径", false)
        .expect_err("路径含 NUL 必须被拒（不能截断成半条路径）");
    assert!(
        matches!(error, Error::Protocol(_)),
        "参数非法应映射为 Protocol，实际: {error:?}"
    );
    println!("✓ start_trace 拒绝含 NUL 的路径：{error}");

    // 正向路径：桥接层后面的 agent 是**真实实现**（不是桩），所以在
    // attach + 全量能力之后，这一对调用应当真的产出一份轨迹文件。
    // 注意能力位：`trace_record` 属于只读基线，但它必须已经下发过。
    let trace_path = std::env::temp_dir().join("pinbridge-loader-trace.pbt");
    let _ = std::fs::remove_file(&trace_path);
    api.start_trace(trace_path.to_str().expect("临时目录应为有效 UTF-8"), true)
        .expect("已附加且持有 trace_record：开始录制应当成功");
    let (records, events) = api.stop_trace().expect("结束录制应当成功");
    println!("✓ 目标内录制：记录 {records} 条 / 事件 {events} 条");

    let bytes = std::fs::read(&trace_path).expect("录制应当真的写出文件");
    assert!(
        bytes.len() >= 8,
        "轨迹文件太小（{} 字节），不可能是合法的 .pbt",
        bytes.len()
    );
    assert_eq!(
        &bytes[..4],
        b"PBT1",
        "轨迹文件必须以 PBT1 魔数开头（收尾写入 SUMMARY/END 之后仍是这个头）"
    );
    assert!(
        records == events,
        "没有事件被折叠时两个计数应当相等（records={records}, events={events}）"
    );
    println!("✓ 轨迹文件 {} 字节，魔数正确", bytes.len());
    let _ = std::fs::remove_file(&trace_path);
}
