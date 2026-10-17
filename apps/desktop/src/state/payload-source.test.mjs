/**
 * `resolvePayloadPlan` 的用例：**默认绝不能是演示数据**。
 *
 * 这条性质用一句话说清：**没被要求的时候，界面不许伪造任何内容**。
 * 所以下面每一条"不该给演示数据"的输入都要单独钉一次 —— 只测
 * "要求的时给演示数据"是不够的：那正是以前那个 bug（把 demo 当兜底）能通过的原因。
 */
import assert from "node:assert/strict";
import test from "node:test";

import { emptyPayload, resolvePayloadPlan } from "./payload-source.mjs";

test("默认（浏览器里、没有选轨迹）是空状态，不是演示数据", () => {
  const plan = resolvePayloadPlan({ tauriAvailable: false, path: "" });
  assert.equal(plan.kind, "empty");
  assert.match(plan.reason, /不在桌面壳里运行/);
});

test("在壳里但还没选轨迹：也是空状态，并说清下一步", () => {
  const plan = resolvePayloadPlan({ tauriAvailable: true, path: "" });
  assert.equal(plan.kind, "empty");
  assert.match(plan.reason, /还没有选择/);
});

test("只给空白路径（空格）同样按没选处理", () => {
  assert.equal(resolvePayloadPlan({ tauriAvailable: true, path: "   " }).kind, "empty");
});

test("选了轨迹且在壳里：走真实数据", () => {
  const plan = resolvePayloadPlan({ tauriAvailable: true, path: "cov.pbt" });
  assert.equal(plan.kind, "real");
  assert.equal(plan.path, "cov.pbt");
});

test("只有显式要求才给演示数据（浏览器里也一样，但要说明是按谁的要求）", () => {
  const plan = resolvePayloadPlan({ tauriAvailable: false, path: "", demoRequested: true });
  assert.equal(plan.kind, "demo");
  assert.match(plan.reason, /按你的要求/);
});

test("显式要求优先于真实路径：翻演示数据时不该被真实轨迹抢走", () => {
  assert.equal(
    resolvePayloadPlan({ tauriAvailable: true, path: "cov.pbt", demoRequested: true }).kind,
    "demo",
  );
});

test("空载荷字段齐全但全为空，且**带一条原因告警**（界面要把它渲染在最上面）", () => {
  const payload = emptyPayload("还没有选择要打开的轨迹（.pbt）");
  assert.equal(payload.records, 0);
  assert.deepEqual(payload.events, []);
  assert.deepEqual(payload.kinds, []);
  assert.equal(payload.summary.dropped, 0);
  assert.equal(payload.warnings.length, 1);
  assert.match(payload.warnings[0], /没有加载轨迹：还没有选择/);
});
