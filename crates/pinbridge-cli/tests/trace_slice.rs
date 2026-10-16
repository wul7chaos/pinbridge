//! `pinbridge slice` 的端到端契约（语义与边界见 ADR 0013）。

use std::path::{Path, PathBuf};
use std::process::Command;

use pinbridge_trace::event::{Event, EventKind};
use pinbridge_trace::pbt::PbtWriter;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pinbridge"))
}

/// 写入事件的 `address` 是**执行写入的那条指令**（与目标范围无关），
/// 写入地址在 `args[0]`。这条约定错了会得到一个看起来正常的空切片。
fn write(sequence: u64, address: u64, size: u64, value: u64, instruction: u64) -> Event {
    Event::new(sequence, EventKind::MemWrite, 7, instruction)
        .with_arg(0, address)
        .with_arg(1, size)
        .with_arg(2, value)
}

fn read(sequence: u64, address: u64, size: u64, instruction: u64) -> Event {
    Event::new(sequence, EventKind::MemRead, 7, instruction)
        .with_arg(0, address)
        .with_arg(1, size)
}

fn executes(sequence: u64, address: u64) -> Event {
    Event::new(sequence, EventKind::BreakpointHit, 7, address)
}

fn write_trace(path: &Path, events: &[Event]) {
    let file = std::fs::File::create(path).expect("建立轨迹文件");
    let mut writer = PbtWriter::create(file, 0).expect("建立写入器");
    writer.meta("{\"target\":\"slice-test\"}").expect("写 meta");
    let mut sequence = 0u64;
    for event in events {
        sequence += 1;
        let mut event = *event;
        event.sequence = sequence;
        writer.write_record(&event).expect("写记录");
    }
    writer.finish().expect("收尾");
}

fn temp_path(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pinbridge-cli-slice-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建立临时目录");
    dir.join("trace.pbt")
}

fn run_slice(path: &Path, extra: &[&str]) -> (String, bool) {
    let output = Command::new(bin())
        .arg("slice")
        .arg(path)
        .args(extra)
        .output()
        .expect("运行 pinbridge slice");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    )
}

/// 脱壳形状：反复改写同一片内存，然后执行它。
#[test]
fn repeated_writes_and_a_later_execution_are_all_reported() {
    let path = temp_path("unpack");
    write_trace(
        &path,
        &[
            write(0, 0x2000, 16, 0xAA, 0x9000),
            write(0, 0x2000, 16, 0xBB, 0x9000),
            write(0, 0x2000, 16, 0xCC, 0x9000),
            read(0, 0x2000, 16, 0x9000),
            executes(0, 0x2004),
        ],
    );
    let (stdout, ok) = run_slice(&path, &["--at", "0x2000", "--len", "16"]);
    assert!(ok, "切片不是错误: {stdout}");
    assert!(stdout.contains("writers=3"), "三个写入者都要在: {stdout}");
    assert!(stdout.contains("overwritten=2"), "{stdout}");
    assert!(stdout.contains("readers=1"), "{stdout}");
    assert!(stdout.contains("first_execution=5"), "{stdout}");
    assert!(stdout.contains("memory_probe=1"), "{stdout}");
    assert!(stdout.contains("不能**说清"), "边界要写在报告里: {stdout}");
}

/// 没有内存写入事件时，必须说"探针没开"，而不是报一条空链。
#[test]
fn without_memory_events_it_says_the_probe_is_off() {
    let path = temp_path("noprobe");
    write_trace(&path, &[executes(0, 0x2000), executes(0, 0x2003)]);
    let (stdout, ok) = run_slice(&path, &["--at", "0x2000", "--len", "16"]);
    assert!(ok);
    assert!(stdout.contains("没有内存写入事件"), "{stdout}");
    assert!(stdout.contains("-pb_mem"), "要指出怎么打开探针: {stdout}");
    assert!(stdout.contains("memory_probe=0"), "{stdout}");
    assert!(stdout.contains("executions=2"), "{stdout}");
}

/// `--at` 缺失或无法解析时给用法，而不是猜一个地址。
#[test]
fn a_missing_address_is_a_usage_error() {
    let path = temp_path("noat");
    write_trace(&path, &[executes(0, 0x2000)]);
    let (stdout, ok) = run_slice(&path, &["--len", "16"]);
    assert!(!ok, "没有地址就不该成功");
    assert!(stdout.is_empty());
}
