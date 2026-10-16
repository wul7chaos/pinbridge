/**
 * 工作台骨架。
 *
 * 现状（M0）：使用本地 mock 数据，不接 Hub。目的是把布局与信息层级先定下来：
 * **哪些信息必须常驻可见**，比"界面上有什么按钮"重要得多。
 *
 * 常驻可见的三件事（都是设计决定，不是装饰）：
 *   1. 控制权持有者与 epoch —— 一眼看出现在是 AI 还是人在驾驶；
 *   2. 轨迹的 `dropped` —— 不能让使用者在不知道丢过数据的前提下看结论；
 *   3. 当前脚本版本（sha256 前 12 位）—— 回答"这个行为是哪一版决定的"。
 */
import { useMemo, useState } from "react";
import type { AuditEntry, EventView, SessionSummary, TraceSummary } from "./api/hub";
import { AuditChain } from "./components/AuditChain";
import { CapabilityPanel } from "./components/CapabilityPanel";
import { EventStream } from "./components/EventStream";
import { SessionBar } from "./components/SessionBar";

const MOCK_SESSION: SessionSummary = {
  session: 1,
  pid: 4242,
  state: "paused",
  image_path: "TARGET.exe",
  lease_holder: "ai:demo-agent",
  scripts: 1,
};

const MOCK_SUMMARY: TraceSummary = {
  records: 71,
  events: 4069,
  repeat_folded: 3999,
  dropped: 0,
  first_sequence: 1,
  last_sequence: 4069,
  truncated: false,
};

function mockEvents(): EventView[] {
  return [
    { seq: 1, kind: "state_change", thread: 0, address: 0x140001000, args: [0x6270_6175_7365_64] },
    { seq: 2, kind: "syscall_enter", thread: 100, address: 0x7ff6_0000_2000, args: [0x40, 0x7ff6_0000_2300] },
    { seq: 8, kind: "repeat", thread: 100, address: 0x7ff6_0000_2000, args: [6, 3], repeats: 6 },
    { seq: 9, kind: "mem_write", thread: 100, address: 0x7ff6_0000_3000, args: [0x7ff6_0000_3000, 8, 0xdead_0000] },
    { seq: 10, kind: "breakpoint_hit", thread: 100, address: 0x1400_01000, args: [0x1400_01000] },
  ];
}

const MOCK_AUDIT: AuditEntry[] = [
  {
    seq: 1,
    at: "2026-10-16T07:56:23Z",
    session: 1,
    actor: { kind: "human", name: "operator" },
    action: "create_session",
    detail: "session=1 image=TARGET.exe arch=x86_64",
    outcome: "ok",
  },
  {
    seq: 3,
    at: "2026-10-16T07:56:23Z",
    session: 1,
    actor: { kind: "ai", name: "demo-agent" },
    action: "control_breakpoint_set",
    detail: "BreakpointSet { address: 0x140001000 } epoch=1",
    outcome: "能力不足: 需要 Breakpoint，当前持有 CapabilitySet(257)",
  },
  {
    seq: 4,
    at: "2026-10-16T07:56:23Z",
    session: 1,
    actor: { kind: "human", name: "operator" },
    action: "grant_capabilities",
    detail: "target=ai:demo-agent → mem_read|mem_write|...|capability_grant",
    outcome: "ok",
  },
];

export default function App() {
  const [following, setFollowing] = useState(true);
  const events = useMemo(mockEvents, []);
  const [auditFilter, setAuditFilter] = useState<"all" | "ai" | "human">("all");

  const visibleAudit = useMemo(
    () => (auditFilter === "all" ? MOCK_AUDIT : MOCK_AUDIT.filter((e) => e.actor.kind === auditFilter)),
    [auditFilter],
  );

  return (
    <div className="app">
      <SessionBar session={MOCK_SESSION} summary={MOCK_SUMMARY} onTakeover={() => undefined} />

      <main className="app__body">
        <section className="panel">
          <header className="panel__header">
            <h2>事件流</h2>
            <label className="panel__toggle">
              <input type="checkbox" checked={following} onChange={(e) => setFollowing(e.target.checked)} />
              跟随最新
            </label>
          </header>
          <EventStream events={events} following={following} />
        </section>

        <section className="panel">
          <header className="panel__header">
            <h2>审计链</h2>
            <select value={auditFilter} onChange={(e) => setAuditFilter(e.target.value as "all" | "ai" | "human")}>
              <option value="all">全部主体</option>
              <option value="ai">仅 AI</option>
              <option value="human">仅人类</option>
            </select>
          </header>
          <AuditChain entries={visibleAudit} />
        </section>
      </main>

      <footer className="app__footer">
        <CapabilityPanel names={["mem_read", "mem_write", "breakpoint", "script_load", "trace_record"]} bits={0x0fff} />
      </footer>
    </div>
  );
}
