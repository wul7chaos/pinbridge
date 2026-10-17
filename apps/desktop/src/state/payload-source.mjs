/**
 * 「这份数据是从哪来的」——**一条纯函数决策**，不碰 React，可被用例钉住。
 *
 * 为什么单独抽出来：这个决策以前散在 hook 里，而且做成了
 * **"拿不到真实数据就用演示数据"** —— 那等于默认给使用者看一份**伪造得
 * 和真实数据一模一样**的东西（假的事件、假的审计链、假的 pid）。
 * 更糟的是它看起来完全正常：人盯着屏幕做判断时，不会去确认这行数字到底是不是真的。
 *
 * 现在的规矩（与 `view-model` 里那条"告警永远在最上面"配套）：
 *
 *   * **默认是空状态** —— 没有真实轨迹就明说"没有加载轨迹"，**不伪造任何内容**；
 *   * 演示数据**只在被显式要求时**出现（URL 上的 `?demo=1`，或界面上那个按钮），
 *     而且仍然会把自己写成一条告警（`useViewPayload` 负责加）；
 *   * 真拿不到真实轨迹时的 empty 状态要带上**原因**，因为"为什么没有"决定了
 *     使用者下一步该做什么。
 */

/**
 * @typedef {{ kind: "real", path: string }
 *   | { kind: "demo", reason: string }
 *   | { kind: "empty", reason: string }} PayloadPlan
 */

/**
 * 决定该去取哪种载荷。
 *
 * @param {{ tauriAvailable?: boolean, path?: string, demoRequested?: boolean }} [input]
 * @returns {PayloadPlan}
 */
export function resolvePayloadPlan(input) {
  const tauriAvailable = Boolean(input?.tauriAvailable);
  const path = typeof input?.path === "string" ? input.path.trim() : "";
  const demoRequested = Boolean(input?.demoRequested);

  // 显式要求优先：使用者要点开演示数据是**知情**的，那就给他看。
  if (demoRequested) {
    return { kind: "demo", reason: "按你的要求显示演示数据" };
  }
  if (!tauriAvailable) {
    // 浏览器里跑的只是界面：既没有 Tauri 命令可用，也**绝不该**用演示数据顶上。
    return {
      kind: "empty",
      reason: "当前不在桌面壳里运行，拿不到轨迹（浏览器只能看界面）",
    };
  }
  if (path === "") {
    return { kind: "empty", reason: "还没有选择要打开的轨迹（.pbt）" };
  }
  return { kind: "real", path };
}

/** 空载荷：字段齐全但**全部为空**，避免组件因为缺字段而报错或隐藏问题。 */
export function emptyPayload(reason) {
  return {
    path: "",
    summary: { records: 0, events: 0, folded: 0, dropped: 0, truncated: false },
    kinds: [],
    events: [],
    records: 0,
    has_more: false,
    warnings: [`没有加载轨迹：${reason}`],
  };
}
