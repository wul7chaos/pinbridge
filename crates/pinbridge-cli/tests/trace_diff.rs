//! `pinbridge diff` 的端到端契约。
//!
//! 为什么要在**二进制层**测一次，而不是只测 `diff_traces` 库函数：
//! 库函数正确但连线错了（参数没接上、展开折叠漏了、退出码反了）是很常见的，
//! 而调用方（AI 或人）用的就是命令行。这一层测的是"**能力真的够得着**"。
//!
//! 轨迹由 `PbtWriter` 现场生成（自写数据，不用任何外部样本）：
//!   * `same-*.pbt`：两份逐条相同的轨迹；
//!   * 其中一份改掉一条记录的参数 ⇒ 数据分岔；
//!   * 再改掉一条记录的地址 ⇒ 控制流分岔（`--ignore-address` 下应消失）。

use std::path::{Path, PathBuf};
use std::process::Command;

use pinbridge_trace::event::{Event, EventKind};
use pinbridge_trace::pbt::PbtWriter;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pinbridge"))
}

fn write_trace(path: &Path, mutate: impl Fn(&mut Vec<Event>)) {
    let mut events = vec![
        Event::new(1, EventKind::SyscallEnter, 7, 0x1400_0010).with_arg(0, 0x40),
        Event::new(2, EventKind::MemWrite, 7, 0x1400_0020)
            .with_arg(0, 0x2000)
            .with_arg(2, 0xAA),
        Event::new(3, EventKind::SyscallExit, 7, 0x1400_0010).with_arg(0, 0x40),
    ];
    mutate(&mut events);
    let file = std::fs::File::create(path).expect("建立轨迹文件");
    let mut writer = PbtWriter::create(file, 0).expect("建立写入器");
    writer.meta("{\"target\":\"diff-test\"}").expect("写 meta");
    for event in &events {
        writer.write_record(event).expect("写记录");
    }
    writer.finish().expect("收尾");
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pinbridge-cli-diff-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建立临时目录");
    dir
}

fn run_diff(left: &Path, right: &Path, extra: &[&str]) -> (String, bool) {
    let mut command = Command::new(bin());
    command.arg("diff").arg(left).arg(right).args(extra);
    let output = command.output().expect("运行 pinbridge diff");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    )
}

/// 逐条相同 ⇒ 结论是「一致」，而且 `agrees_up_to` 等于记录数。
#[test]
fn identical_traces_report_identical() {
    let dir = temp_dir("same");
    let left = dir.join("left.pbt");
    let right = dir.join("right.pbt");
    write_trace(&left, |_| {});
    write_trace(&right, |_| {});

    let (stdout, ok) = run_diff(&left, &right, &[]);
    assert!(ok, "差分不是错误，应当以 0 退出: {stdout}");
    assert!(
        stdout.contains("PB_DIFF divergence=一致 agrees_up_to=3"),
        "机器行必须给出准确结论: {stdout}"
    );
    assert!(stdout.contains("warnings=0"), "这份数据没毛病: {stdout}");
}

/// 参数不同 ⇒ **数据分岔**，并指出是哪个参数。
#[test]
fn a_parameter_change_reports_a_data_divergence_and_names_the_field() {
    let dir = temp_dir("data");
    let left = dir.join("left.pbt");
    let right = dir.join("right.pbt");
    write_trace(&left, |_| {});
    write_trace(&right, |events| {
        events[1] = events[1].with_arg(2, 0xBB);
    });

    let (stdout, ok) = run_diff(&left, &right, &[]);
    assert!(ok);
    assert!(
        stdout.contains("PB_DIFF divergence=数据分岔"),
        "必须分类，不能只说「某条不一样」: {stdout}"
    );
    assert!(stdout.contains("字段 arg3"), "要指名参数: {stdout}");
    assert!(
        stdout.contains("agrees_up_to=1"),
        "共同前缀是 1 条: {stdout}"
    );
    assert!(
        stdout.contains("first_sequence=2"),
        "分岔位置是第 2 条: {stdout}"
    );
}

/// 地址不同 ⇒ 控制流分岔；`--ignore-address` 下应变成「一致」。
///
/// 这是跨运行对比的日常情形（ASLR 让地址每次都不同）。
#[test]
fn address_differences_are_control_flow_noise_that_can_be_ignored() {
    let dir = temp_dir("addr");
    let left = dir.join("left.pbt");
    let right = dir.join("right.pbt");
    write_trace(&left, |_| {});
    write_trace(&right, |events| {
        for (i, event) in events.iter_mut().enumerate() {
            event.address = 0x7F00_0000 + (i as u64) * 0x1000;
        }
    });

    let (noisy, ok) = run_diff(&left, &right, &[]);
    assert!(ok);
    assert!(
        noisy.contains("PB_DIFF divergence=控制流分岔 agrees_up_to=0"),
        "不忽略地址时第一处就是地址差异: {noisy}"
    );

    let (clean, ok) = run_diff(&left, &right, &["--ignore-address"]);
    assert!(ok);
    assert!(
        clean.contains("PB_DIFF divergence=一致"),
        "--ignore-address 之后行为一致: {clean}"
    );
}

/// 写坏的轨迹（缺 END）⇒ 结论照出，但**必须带告警**。
///
/// 这条守的是最危险的情形：拿着不完整的数据下判决。
#[test]
fn a_truncated_trace_still_diffs_but_warns_that_the_data_is_incomplete() {
    let dir = temp_dir("truncated");
    let left = dir.join("left.pbt");
    let right = dir.join("right.pbt");
    write_trace(&left, |_| {});
    write_trace(&right, |_| {});
    // 砍掉右轨迹的尾巴（END 与部分汇总），模拟录制被中断。
    let bytes = std::fs::read(&right).expect("读轨迹");
    std::fs::write(&right, &bytes[..bytes.len() - 6]).expect("写回截断轨迹");

    let (stdout, ok) = run_diff(&left, &right, &[]);
    assert!(ok, "有告警不等于失败: {stdout}");
    assert!(
        stdout.contains("⚠") && stdout.contains("warnings=1"),
        "截断必须产生告警: {stdout}"
    );
    assert!(
        stdout.contains("先处理告警再看分岔"),
        "告警要在结尾再提一次（报告很长时会滚屏）: {stdout}"
    );
}
