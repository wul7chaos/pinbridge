/**
 * 载荷的来源：**真实数据优先，演示数据必须自己说出来**。
 *
 * 这一条是工作台里最容易被忽略、后果最严重的问题：**演示数据与真实数据长得一样**。
 * 一个人盯着屏幕做判断时，不会去确认"这行数字到底是不是真的"。所以：
 *
 *   * 能用 Tauri 命令拿到真实轨迹就用真实的 (`source: "tauri"`)；
 *   * 拿不到就退到演示数据，但**必须**带上 `source: "demo"`，
 *     界面会把这件事当成一条告警显示（与其他告警同一个位置：最上面）。
 *
 * 换句话说：这个文件存在的意义不是"取数据"，而是**不让演示数据冒充真实数据**。
 */
import { useCallback, useEffect, useState } from "react";

import { buildWorkbenchState, type WorkbenchState } from "./view-model";

/** 载荷来源。 */
export type PayloadSource = "tauri" | "demo" | "loading";

export interface ViewPayloadResult {
  state: WorkbenchState;
  source: PayloadSource;
  reload: (path?: string) => void;
}

/**
 * 演示载荷（**不是** mock 数据的随意编造：字段与 `pinbridge view --json` 一致，
 * 数值取自真实轨迹的形态，这样组件不会因为"演示数据缺字段"而隐藏掉真实问题）。
 */
export function demoPayload(): object {
  return {
    path: "（演示数据）demo.pbt",
    summary: { records: 9728, events: 9728, folded: 0, dropped: 0, truncated: false },
    kinds: [
      { kind: "mem_read", count: 6410 },
      { kind: "mem_write", count: 3035 },
      { kind: "syscall_enter", count: 135 },
      { kind: "syscall_exit", count: 147 },
      { kind: "breakpoint_hit", count: 1 },
    ],
    events: [
      { seq: 1, kind: "syscall_enter", thread: 0, address: 0x7ffee5981f12, args: [0x40] },
      { seq: 2, kind: "syscall_exit", thread: 0, address: 0x7ffee5981f14, args: [] },
      { seq: 3, kind: "mem_write", thread: 0, address: 0x7ffee5982752, repeats: 300, args: [] },
    ],
    records: 9728,
    has_more: true,
    coverage: { sites: 1452, skipped: 245, unreadable: 0, filtered: 0, capped: true },
    warnings: [],
  };
}

/** Tauri 是否在场（浏览器里跑 `npm run dev` 时不在）。 */
function tauriAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * 取载荷。真实优先，演示必须自报家门。
 *
 * @param initialPath 启动时要打开的轨迹路径（为空则用演示数据）
 */
export function useViewPayload(initialPath = ""): ViewPayloadResult {
  const [state, setState] = useState<WorkbenchState>(() =>
    buildWorkbenchState(demoPayload(), { lease: { holder: "ai:demo-agent", epoch: 1 } }),
  );
  const [source, setSource] = useState<PayloadSource>("loading");

  const reload = useCallback((path?: string) => {
    const target = path ?? initialPath;
    if (!tauriAvailable() || target === "") {
      // 演示数据：**在模型里留下一条告警**，界面会把它渲染在最上面。
      const demo = buildWorkbenchState(
        { ...demoPayload(), warnings: ["当前显示的是**演示数据**（没有连接真实轨迹）"] },
        { lease: { holder: "ai:demo-agent", epoch: 1 } },
      );
      setState(demo);
      setSource("demo");
      return;
    }
    void (async () => {
      try {
        // 动态导入：不在 Tauri 里跑时不该因为缺少这个包而整个界面挂掉。
        const { invoke } = await import("@tauri-apps/api/core");
        const payload = (await invoke("view_trace", { path: target, head: 200 })) as
          | object
          | string;
        const parsed = typeof payload === "string" ? JSON.parse(payload) : payload;
        setState(buildWorkbenchState(parsed));
        setSource("tauri");
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        // 失败也要说清两件事：**为什么失败**，以及**现在看的是演示数据**。
        setState(
          buildWorkbenchState(
            { ...demoPayload(), warnings: [`读取真实轨迹失败：${message}`] },
            { lease: { holder: "", epoch: 0 } },
          ),
        );
        setState((previous) => ({
          ...previous,
          warnings: [...previous.warnings, "当前显示的是**演示数据**"],
        }));
        setSource("demo");
      }
    })();
  }, [initialPath]);

  useEffect(() => reload(), [reload]);

  return { state, source, reload };
}
