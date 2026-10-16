//! `pinbridge calls` 的端到端契约（规则见 ADR 0012）。
//!
//! 与 `trace_diff.rs` 同样的理由：库函数对、连线错（事件字段接错、展开漏了、
//! 机器行参数放反）是很常见的，而调用方用的就是命令行。
//!
//! 轨迹现场生成：一段 `call 0x1000 → 0x2000` + `ret 0x2008 → 0x1005`
//! （返回地址正好是调用指令 + 5，即 x86-64 `call rel32` 的长度）。

use std::path::{Path, PathBuf};
use std::process::Command;

use pinbridge_trace::event::{Event, EventKind};
use pinbridge_trace::pbt::PbtWriter;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pinbridge"))
}

/// 一条转移事件（`RegWrite` 形状，见 ADR 0012）。
fn transfer(source: u64, target: u64, code: u64) -> Event {
    Event::new(1, EventKind::RegWrite, 7, source)
        .with_arg(0, 0)
        .with_arg(1, target)
        .with_arg(2, code)
}

fn code_at(address: u64) -> Event {
    Event::new(1, EventKind::BreakpointHit, 7, address)
}

fn write_trace(path: &Path, events: &[Event]) {
    let file = std::fs::File::create(path).expect("建立轨迹文件");
    let mut writer = PbtWriter::create(file, 0).expect("建立写入器");
    writer.meta("{\"target\":\"calls-test\"}").expect("写 meta");
    for (index, event) in events.iter().enumerate() {
        // 序号必须严格递增（写入器会拒掉非递增的输入，这是第 22 轮加的保证）。
        // 事件本身用序号 1 构造，这里统一重排，免得测试数据被序号噪音淹没。
        let mut event = *event;
        event.sequence = index as u64 + 1;
        writer.write_record(&event).expect("写记录");
    }
    writer.finish().expect("收尾");
}

fn temp_path(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pinbridge-cli-calls-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建立临时目录");
    dir.join("trace.pbt")
}

fn run_calls(path: &Path, extra: &[&str]) -> (String, bool) {
    let output = Command::new(bin())
        .arg("calls")
        .arg(path)
        .args(extra)
        .output()
        .expect("运行 pinbridge calls");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    )
}

/// 一段完整的调用/返回 ⇒ 一条调用边、一条返回边、且被算作**配对**。
#[test]
fn a_call_and_its_return_produce_a_paired_graph() {
    let path = temp_path("paired");
    write_trace(
        &path,
        &[
            code_at(0x1000),
            code_at(0x1005),
            code_at(0x2000),
            code_at(0x2008),
            transfer(0x1000, 0x2000, 1),
            transfer(0x2008, 0x1005, 2),
        ],
    );
    let (stdout, ok) = run_calls(&path, &["--top", "4"]);
    assert!(ok, "建图不是错误: {stdout}");
    assert!(
        stdout.contains("call_edges=1 paired=1"),
        "一条调用边且配对: {stdout}"
    );
    assert!(
        stdout.contains("call 0x1000 → 0x2000"),
        "边要打出来: {stdout}"
    );
    assert!(
        stdout.contains("有配对的返回"),
        "配对是本模块最硬的置信度信号，必须报出来: {stdout}"
    );
}

/// 没有转移事件时，输出必须说**为什么**是空的，而不是给一张空图。
#[test]
fn a_trace_without_transfers_says_why_the_graph_is_empty() {
    let path = temp_path("empty");
    write_trace(&path, &[code_at(0x1000), code_at(0x2000)]);
    let (stdout, ok) = run_calls(&path, &[]);
    assert!(ok);
    assert!(stdout.contains("没有转移事件"), "{stdout}");
    assert!(
        stdout.contains("-pb_transfers"),
        "要指出怎么打开探针: {stdout}"
    );
}

/// 目标没在本轨迹里出现 ⇒ 保留但标注未验证；`--strict-code-targets` 才丢。
#[test]
fn unseen_targets_are_kept_unverified_unless_strict_is_asked_for() {
    let path = temp_path("unseen");
    write_trace(&path, &[code_at(0x1000), transfer(0x1000, 0xDEAD_BEEF, 1)]);
    let (loose, ok) = run_calls(&path, &[]);
    assert!(ok);
    assert!(loose.contains("call_edges=1"), "默认保留: {loose}");
    assert!(loose.contains("未在本轨迹里出现"), "要标注未验证: {loose}");

    let (strict, ok) = run_calls(&path, &["--strict-code-targets"]);
    assert!(ok);
    assert!(strict.contains("call_edges=0"), "严格模式下丢掉: {strict}");
    assert!(strict.contains("目标不是代码"), "要说清拒因: {strict}");
}
