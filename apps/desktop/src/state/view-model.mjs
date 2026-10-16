/**
 * 工作台的视图模型（纯逻辑，零依赖）。
 *
 * 为什么把"界面该怎么显示"写成可测的函数，而不是散在组件里：
 * `apps/desktop/README.md` 里有三条**硬要求**，它们都是"人不能被蒙"的问题：
 *
 *   1. **`dropped` 必须常驻可见**，非零时变黄 —— 人不能"不知道这份结论建立在
 *      丢过数据的基础上"；
 *   2. **控制权持有者与 epoch 常驻可见** —— 一眼看出现在是 AI 还是人在驾驶；
 *   3. **接管按钮不做二次确认**（接管本身可恢复；加确认只会让人在需要它的那一刻犹豫）。
 *
 * 这三条写在 README 里就只是文档；写在这里就能被用例钉住。组件再薄，
 * 也很难让"常驻可见"这件事悄悄退化。
 *
 * 逻辑全是纯函数：输入是 `pinbridge view --json` 的载荷（或 Hub 的同形响应），
 * 输出是界面直接渲染的结构。因此它能在**没有任何 Node 依赖**的情况下被验证。
 */

/** 区块顺序是**固定**的：告警永远第一。界面不自己排顺序。 */
export const SECTION_ORDER = ["warnings", "status", "lease", "coverage", "events", "kinds"];

/**
 * 把地址格式化成 `0x…`（与 CLI 的机器行一致）。
 * @param {number} value
 */
export function formatAddress(value) {
  if (!Number.isFinite(value)) {
    return "0x?";
  }
  return `0x${Math.trunc(value).toString(16)}`;
}

/**
 * 折叠展示：没有重复时不显示后缀（而不是显示 `×1` —— 那会让真重复被淹没）。
 * @param {number|undefined} repeats
 */
export function formatRepeats(repeats) {
  if (repeats === undefined || repeats === null || repeats <= 1) {
    return "";
  }
  return ` ×${repeats}`;
}

/**
 * 丢弃计数的展示。
 *
 * **非零必须显眼**：这是"结论建立在丢过数据的基础上"的唯一提示。
 * 注意不能省略 0 的情况 —— 常驻可见的意思是"这一格一直在"，字段消失比显示 0 更糟。
 * @param {number} dropped
 */
export function formatDropped(dropped) {
  const value = Number.isFinite(dropped) ? dropped : 0;
  return { text: `丢弃 ${value}`, critical: value !== 0 };
}

/**
 * 控制权租约的展示。
 * @param {{holder?: string, epoch?: number}|undefined} lease
 */
export function formatLease(lease) {
  if (lease === undefined || lease === null || !lease.holder) {
    // **不能空着**：没有租约本身是一条信息（此刻没人驾驶，或者连接掉了）。
    return { text: "无租约（没有人持有控制权）", holder: "", epoch: 0, isHuman: false };
  }
  const holder = String(lease.holder);
  return {
    text: `${holder}（epoch ${lease.epoch ?? 0}）`,
    holder,
    epoch: Number(lease.epoch ?? 0),
    isHuman: holder.startsWith("human:"),
  };
}

/**
 * 生成界面状态。
 *
 * @param {object} payload `pinbridge view --json` 的输出
 * @param {{lease?: {holder?: string, epoch?: number}, takeoverEnabled?: boolean}} [options]
 */
export function buildWorkbenchState(payload, options = {}) {
  if (payload === null || typeof payload !== "object") {
    // 载荷本身坏了也要能显示：给一条明确的告警，而不是白屏。
    return {
      sections: SECTION_ORDER,
      warnings: ["视图载荷无法解析：这不是「没有数据」，而是数据没能变成视图。"],
      status: null,
      lease: formatLease(undefined),
      coverage: null,
      events: [],
      kinds: [],
      records: 0,
      takeoverEnabled: false,
    };
  }

  const warnings = Array.isArray(payload.warnings) ? [...payload.warnings] : [];
  const summary = payload.summary ?? null;
  const records = payload.records ?? summary?.records ?? 0;
  const dropped = summary?.dropped ?? 0;

  const status = {
    records,
    events: summary?.events ?? 0,
    folded: summary?.folded ?? 0,
    foldRatio: summary
      ? summary.records + summary.folded === 0
        ? 0
        : summary.folded / (summary.records + summary.folded)
      : 0,
    dropped: formatDropped(dropped),
    // **原始计数也要留着**：界面如果只拿得到 `{text, critical}`，就很容易在
    // 需要数字的地方凑一个（比如"非零就当 1"）—— 那就是把真实值换成假值。
    droppedRaw: dropped,
    // 摘要不存在是 Pin 路径上的常态（工具收尾被卸载保护拒绝）⇒ 界面要说清是
    // "没有摘要"，而不是让人以为"没有数据"。
    summaryText: summary ? null : "无摘要（见告警）",
  };

  const coverage = payload.coverage
    ? {
        capped: payload.coverage.capped === true,
        sites: payload.coverage.sites ?? 0,
        skipped: payload.coverage.skipped ?? 0,
        unreadable: payload.coverage.unreadable ?? 0,
        filtered: payload.coverage.filtered ?? 0,
        text:
          `插装点 ${payload.coverage.sites ?? 0}、跳过 ${payload.coverage.skipped ?? 0}、` +
          `读不出 ${payload.coverage.unreadable ?? 0}、过滤 ${payload.coverage.filtered ?? 0}` +
          (payload.coverage.capped ? "（上限用尽，之后的事件不存在）" : ""),
      }
    : null;

  const eventRows = (Array.isArray(payload.events) ? payload.events : []).map((event) => ({
    seq: event.seq,
    kind: event.kind,
    // 认不出的类型也要显示出来（轨迹可能来自更新的录制方）：
    // 把它们过滤掉会让人以为"这类事件没发生过"。
    thread: event.thread ?? 0,
    address: formatAddress(event.address),
    repeats: formatRepeats(event.repeats),
    args: Array.isArray(event.args) ? event.args.map(formatAddress) : [],
  }));

  const takeoverEnabled = options.takeoverEnabled !== false;
  const lease = formatLease(options.lease);
  if (!lease.holder) {
    // 没有租约时接管按钮的意义不同（那是"开始驾驶"而不是"抢回"）——
    // 但**按钮永远在**：README 说接管不做二次确认，也意味着它不能被藏起来。
    warnings.push("当前没有控制权租约：接管按钮会直接开始驾驶（这一步不需要确认）。");
  }

  return {
    // 顺序由 SECTION_ORDER 固定，界面按它渲染 ⇒ 告警永远在最上面。
    sections: SECTION_ORDER,
    warnings,
    status,
    lease,
    coverage,
    events: eventRows,
    kinds: (Array.isArray(payload.kinds) ? payload.kinds : []).map((kind) => ({
      kind: kind.kind,
      count: kind.count,
    })),
    records,
    takeoverEnabled,
  };
}
