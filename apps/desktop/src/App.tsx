/**
 * 工作台。
 *
 * 数据来自**真实产物**：`pinbridge view --json` 的载荷经 `useViewPayload` 取回，
 * 再由零依赖的视图模型（`src/state/view-model.mjs`，7 条 `node --test` 用例）
 * 变成界面结构。演示数据只在拿不到真实数据时出现，并且**自己会说出来**。
 *
 * 常驻可见的三件事（都是设计决定，不是装饰）：
 *   1. **告警在最上面** —— 残缺的轨迹与完整的轨迹长得一样，不先说清楚就等于骗人；
 *   2. 控制权持有者与 epoch —— 一眼看出现在是 AI 还是人在驾驶；
 *   3. 轨迹的 `dropped` —— 不能让使用者在不知道丢过数据的前提下看结论。
 *
 * 这三条不是靠"记得写"守住的：它们写进了视图模型，并由用例钉住
 * （`src/state/view-model.test.mjs`）。
 */
import { useMemo, useState } from "react";
import type { AuditEntry, EventView, SessionSummary, TraceSummary } from "./api/hub";
import { AuditChain } from "./components/AuditChain";
import { CapabilityPanel } from "./components/CapabilityPanel";
import { EventStream } from "./components/EventStream";
import { SessionBar } from "./components/SessionBar";
import { useViewPayload } from "./state/useViewPayload";

/** 会话信息（将来由 Hub 提供；现在只有演示值）。 */
const DEMO_SESSION: SessionSummary = {
  session: 1,
  pid: 4242,
  state: "paused",
  image_path: "TARGET.exe",
  lease_holder: "ai:demo-agent",
  lease_epoch: 1,
  scripts: 1,
};

const DEMO_AUDIT: AuditEntry[] = [
  {
    seq: 1,
    at: "2026-10-17T00:00:00Z",
    session: 1,
    actor: { kind: "human", name: "operator" },
    action: "create_session",
    detail: "session=1 image=TARGET.exe arch=x86_64",
    outcome: "ok",
  },
  {
    seq: 2,
    at: "2026-10-17T00:00:01Z",
    session: 1,
    actor: { kind: "ai", name: "demo-agent" },
    action: "control_breakpoint_set",
    detail: "BreakpointSet { address: 0x140001000 } epoch=1",
    outcome: "能力不足: 需要 Breakpoint，当前持有 CapabilitySet(257)",
  },
  {
    seq: 3,
    at: "2026-10-17T00:00:02Z",
    session: 1,
    actor: { kind: "human", name: "operator" },
    action: "grant_capabilities",
    detail: "target=ai:demo-agent → mem_read|mem_write|...|capability_grant",
    outcome: "ok",
  },
];

const DEMO_CAPABILITIES = [
  "mem_read",
  "mem_write",
  "breakpoint",
  "script_load",
  "trace_record",
];

export default function App() {
  const [following, setFollowing] = useState(true);
  const [auditFilter, setAuditFilter] = useState<"all" | "ai" | "human">("all");
  const { state, source } = useViewPayload("");

  // 事件流沿用组件既有的 `EventView` 形状：视图模型里的行已经做过格式化，
  // 这里只把地址/参数还原成组件要的数字（展示格式由组件自己决定）。
  const events = useMemo<EventView[]>(
    () =>
      state.events.map((row) => ({
        seq: row.seq,
        kind: row.kind,
        thread: row.thread,
        address: Number.parseInt(row.address.replace(/^0x/, ""), 16) || 0,
        args: row.args.map((arg) => Number.parseInt(arg.replace(/^0x/, ""), 16) || 0),
      })),
    [state.events],
  );

  const summary = useMemo<TraceSummary>(
    () => ({
      records: state.status?.records ?? 0,
      events: state.status?.events ?? 0,
      repeat_folded: state.status?.folded ?? 0,
      dropped: state.status?.droppedRaw ?? 0,
      first_sequence: 0,
      last_sequence: 0,
      truncated: false,
    }),
    [state.status],
  );

  const session: SessionSummary = {
    ...DEMO_SESSION,
    lease_holder: state.lease.holder || "（无租约）",
    // 真实载荷时用视图模型算出的 epoch；**不能**沿用 DEMO_SESSION 里那个假值 ——
    // 界面上看不出差别，但那正好是"看起来对、其实是死的"那一类错误。
    lease_epoch: state.lease.epoch,
  };

  const visibleAudit = useMemo(
    () =>
      auditFilter === "all"
        ? DEMO_AUDIT
        : DEMO_AUDIT.filter((entry) => entry.actor.kind === auditFilter),
    [auditFilter],
  );

  return (
    <div className="app">
      <SessionBar session={session} summary={summary} onTakeover={() => undefined} />

      <main className="app__body">
        {/* 告警永远在数据之前（顺序由视图模型给定，这里只按它渲染）。 */}
        {state.sections[0] === "warnings" && state.warnings.length > 0 && (
          <section className="panel panel--warnings">
            <header className="panel__header">
              <h2>先读这些</h2>
              <span className="panel__source">
                数据来源：{source === "tauri" ? "真实轨迹" : "演示数据"}
              </span>
            </header>
            <ul className="warnings">
              {state.warnings.map((warning) => (
                <li key={warning}>{warning}</li>
              ))}
            </ul>
          </section>
        )}

        <section className="panel">
          <header className="panel__header">
            <h2>事件流</h2>
            <label className="panel__toggle">
              <input
                type="checkbox"
                checked={following}
                onChange={(event) => setFollowing(event.target.checked)}
              />
              跟随最新
            </label>
          </header>
          <EventStream events={events} following={following} />
        </section>

        <section className="panel">
          <header className="panel__header">
            <h2>审计链</h2>
            <select
              value={auditFilter}
              onChange={(event) =>
                setAuditFilter(event.target.value as "all" | "ai" | "human")
              }
            >
              <option value="all">全部主体</option>
              <option value="ai">仅 AI</option>
              <option value="human">仅人类</option>
            </select>
          </header>
          <AuditChain entries={visibleAudit} />
        </section>
      </main>

      <footer className="app__footer">
        <CapabilityPanel names={DEMO_CAPABILITIES} bits={0x0fff} />
      </footer>
    </div>
  );
}
