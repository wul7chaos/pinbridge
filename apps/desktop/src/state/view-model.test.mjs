/**
 * 视图模型的用例（`node --test`，零依赖）。
 *
 * 这些用例守的是 `apps/desktop/README.md` 里那三条硬要求。写在这里的理由很直接：
 * **文档会被忘记，用例不会。**
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  buildWorkbenchState,
  formatAddress,
  formatDropped,
  formatLease,
  formatRepeats,
  SECTION_ORDER,
} from "./view-model.mjs";

/** 一份真实形状的 `pinbridge view --json` 载荷（取自真实 Pin 轨迹的字段）。 */
function realPayload(overrides = {}) {
  return {
    path: "trace.pbt",
    summary: { records: 9728, events: 9728, folded: 0, dropped: 0, truncated: false },
    kinds: [{ kind: "mem_read", count: 6410 }],
    events: [
      { seq: 1, kind: "syscall_enter", thread: 0, address: 0x7ffee5981f12, args: [0x40] },
      { seq: 2, kind: "mem_write", thread: 0, address: 0x7ffee5981f14, repeats: 300, args: [] },
    ],
    records: 9728,
    has_more: true,
    coverage: { sites: 1452, skipped: 245, unreadable: 0, filtered: 0, capped: true },
    warnings: ["轨迹没有收尾段（没有 END）", "录制时探针达到事件上限并停止采集"],
    ...overrides,
  };
}

test("告警永远排在第一个区块（界面的渲染顺序由模型决定）", () => {
  const state = buildWorkbenchState(realPayload(), { lease: { holder: "ai:agent-7", epoch: 3 } });
  assert.deepEqual(state.sections, SECTION_ORDER);
  assert.equal(state.sections[0], "warnings", "告警必须是第一个区块");
  assert.equal(state.warnings.length, 2, "告警原样带出，不合并、不截断");
  assert.match(state.warnings[0], /没有收尾段/);
});

test("载荷坏掉时给出明确告警，而不是白屏", () => {
  const state = buildWorkbenchState(null);
  assert.equal(state.sections[0], "warnings");
  assert.match(state.warnings[0], /无法解析/);
  assert.equal(state.status, null);
  assert.deepEqual(state.events, []);
});

test("dropped 常驻可见：0 也要显示，非 0 必须标记为危险", () => {
  const clean = buildWorkbenchState(realPayload(), { lease: { holder: "human:op", epoch: 1 } });
  assert.equal(clean.status.dropped.text, "丢弃 0", "0 也要有文字（字段消失比显示 0 更糟）");
  assert.equal(clean.status.dropped.critical, false);

  const lossy = buildWorkbenchState(
    realPayload({ summary: { records: 10, events: 12, folded: 0, dropped: 7952, truncated: false } }),
    { lease: { holder: "human:op", epoch: 1 } },
  );
  assert.equal(lossy.status.dropped.critical, true, "非 0 必须显眼");
  assert.equal(lossy.status.droppedRaw, 7952, "原始计数必须原样保留，界面不该自己凑数字");
  assert.match(lossy.status.dropped.text, /7952/);

  // 即使没有摘要（Pin 路径上的常态），这一格也要在
  const noSummary = buildWorkbenchState(realPayload({ summary: null }), {
    lease: { holder: "human:op", epoch: 1 },
  });
  assert.equal(noSummary.status.dropped.text, "丢弃 0");
  assert.equal(noSummary.status.summaryText, "无摘要（见告警）");
});

test("控制权与 epoch 常驻可见；没有租约时也要说清楚", () => {
  const ai = buildWorkbenchState(realPayload(), { lease: { holder: "ai:agent-7", epoch: 12 } });
  assert.equal(ai.lease.isHuman, false);
  assert.match(ai.lease.text, /ai:agent-7（epoch 12）/);

  const human = buildWorkbenchState(realPayload(), { lease: { holder: "human:op", epoch: 12 } });
  assert.equal(human.lease.isHuman, true);

  const none = buildWorkbenchState(realPayload());
  assert.equal(none.lease.holder, "");
  assert.match(none.lease.text, /无租约/, "空着不行：这是一条信息");
  assert.ok(
    none.warnings.some((warning) => warning.includes("接管按钮")),
    "没有租约时要说明接管按钮意味着什么: " + JSON.stringify(none.warnings),
  );
  assert.equal(none.takeoverEnabled, true, "接管按钮永远在（不做二次确认，也不藏起来）");
});

test("认不出的类型也要显示出来（过滤掉会让人以为没发生过）", () => {
  const state = buildWorkbenchState(
    realPayload({
      events: [{ seq: 1, kind: "future_kind", thread: 2, address: 4096, args: [] }],
    }),
    { lease: { holder: "human:op", epoch: 1 } },
  );
  assert.equal(state.events.length, 1);
  assert.equal(state.events[0].kind, "future_kind");
  assert.equal(state.events[0].address, "0x1000");
});

test("覆盖缺口进入视图，并写明上限用尽意味着什么", () => {
  const state = buildWorkbenchState(realPayload(), { lease: { holder: "human:op", epoch: 1 } });
  assert.equal(state.coverage.capped, true);
  assert.equal(state.coverage.skipped, 245);
  assert.match(state.coverage.text, /跳过 245/);
  assert.match(state.coverage.text, /上限用尽/, "要说清后果，不只是数字");
});

test("格式化的小约定", () => {
  assert.equal(formatAddress(0x7ffee5981f12), "0x7ffee5981f12");
  assert.equal(formatAddress(Number.NaN), "0x?");
  assert.equal(formatRepeats(undefined), "");
  assert.equal(formatRepeats(1), "", "×1 是噪声");
  assert.equal(formatRepeats(300), " ×300");
  assert.deepEqual(formatDropped(0), { text: "丢弃 0", critical: false });
  assert.deepEqual(formatDropped(7), { text: "丢弃 7", critical: true });
  assert.equal(formatLease(undefined).isHuman, false);
  assert.equal(formatLease({ holder: "human:op", epoch: 0 }).isHuman, true);
});
