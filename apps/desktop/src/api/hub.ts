/**
 * Hub 调用的类型化包装。
 *
 * 传输可以是两种（见 apps/desktop/README.md）：
 *   1. Tauri 进程内调用（桌面端自带 Hub）；
 *   2. 本机回环 TCP 上的 NDJSON（连一个已在跑的 Hub）。
 *
 * 无论哪种，**只允许回环**：把调试能力暴露到网络上不是可配置项。
 *
 * 字段名与 `crates/pinbridge-proto/src/frames.rs` 一一对应。
 * 这里刻意不做"顺手改个更好听的字段名"——两侧字段名不同会让
 * 联调退化成人工映射，而人工映射必然出错。
 */

/** 会话摘要（对应 Rust `SessionSummary`）。 */
export interface SessionSummary {
  session: number;
  pid: number;
  /** 会话状态机：created / attached / running / paused / detached / closed / failed */
  state: string;
  image_path: string;
  /** 控制权持有者，形如 `human:operator` / `ai:agent-7` */
  lease_holder: string;
  /**
   * 租约代次（epoch）。
   *
   * 为什么要**和持有者一起**常驻显示：光看"现在是 AI 在驾驶"不够 ——
   * 老界面会显示一个**已经过期**的持有者。epoch 变了就说明控制权换过人，
   * 而使用者必须一眼看出自己看到的这份数据属于哪一代。
   */
  lease_epoch: number;
  scripts: number;
}

/** 能力集（对应 Rust `CapabilitySet` 的 JSON 形态）。 */
export interface CapabilitySet {
  bits: number;
  names: string[];
}

/** 事件视图（对应 Rust `EventView`）。 */
export interface EventView {
  seq: number;
  kind: string;
  thread: number;
  address: number;
  /** 二进制里是定长 8 项，JSON 层去掉了末尾的零 */
  args: number[];
  /** 折叠出来的重复次数；非折叠事件省略（按 1 理解） */
  repeats?: number;
}

/** 轨迹摘要（对应 Rust `Summary`）。 */
export interface TraceSummary {
  records: number;
  events: number;
  repeat_folded: number;
  /** 非 0 即代表轨迹不完整——界面上必须常驻可见 */
  dropped: number;
  first_sequence: number;
  last_sequence: number;
  truncated: boolean;
}

/** 审计记录（对应 Rust `AuditEntry`）。 */
export interface AuditEntry {
  seq: number;
  at: string;
  session?: number;
  actor: { kind: string; name: string };
  action: string;
  detail: string;
  outcome: string;
}

/** Hub 客户端接口。Tauri 与 NDJSON 两种实现都满足它。 */
export interface HubClient {
  listSessions(): Promise<SessionSummary[]>;
  queryEvents(session: number, from: number, limit: number, foldRepeats: boolean): Promise<{
    items: EventView[];
    next: number;
    /** 必须展示：不能让使用者在不知道丢过数据的前提下看结论 */
    dropped: number;
  }>;
  provenance(session: number): Promise<AuditEntry[]>;
  capabilities(): Promise<CapabilitySet>;
  /** 人类硬接管：收回租约并推进 epoch */
  takeover(session: number): Promise<number>;
  grantCapabilities(session: number, target: string, capabilities: string[]): Promise<CapabilitySet>;
}

/** 把能力位转成 `0x...` 形式，便于与 CLI 输出对照。 */
export function formatCapabilityBits(bits: number): string {
  return `0x${bits.toString(16).toUpperCase().padStart(4, "0")}`;
}

/** 折叠率（与 Rust `Summary::fold_ratio` 同定义）。 */
export function foldRatio(summary: TraceSummary): number {
  const total = summary.records + summary.repeat_folded;
  return total === 0 ? 0 : summary.repeat_folded / total;
}
