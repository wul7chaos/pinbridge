/**
 * 载荷的来源：**真实优先，没有就空着，演示数据必须被显式要求**。
 *
 * 这个文件的存在意义不是"取数据"，而是**不让界面伪造内容**。
 * 以前的规则是"拿不到真实数据就退到演示数据"——那条规则错在：
 * 演示数据与真实数据长得一模一样（假事件、假审计链、假 pid），
 * 而人盯着屏幕做判断时不会去确认这行数字是不是真的。
 *
 * 现在的规则写在 `payload-source.mjs`（纯函数，带用例）里：
 *
 *   * 没被要求 → **空状态**，并说清"为什么没有"（没有桌面壳 / 还没选轨迹 / 读取失败）；
 *   * 被显式要求（`?demo=1` 或界面上的按钮）→ 演示数据，且**照旧把自己写成告警**；
 *   * 读取失败 → 空状态 + 失败原因，**绝不**退到演示数据。
 */
import { useCallback, useEffect, useState } from "react";

import { buildWorkbenchState, type WorkbenchState } from "./view-model";
// 注意：这里**不带** `.mjs` 扩展名 —— 带扩展名时 TS 会去找 `payload-source.d.mts`，
// 而不带时才认 `payload-source.d.ts`（与项目里 `./view-model` 的写法一致）。
import { emptyPayload, resolvePayloadPlan } from "./payload-source";

/** 载荷来源。`empty` = 明确没有数据（不是"还在加载"）。 */
export type PayloadSource = "tauri" | "demo" | "empty" | "loading";

export interface ViewPayloadResult {
  state: WorkbenchState;
  source: PayloadSource;
  /** 重新取（可指定轨迹路径）。 */
  reload: (path?: string) => void;
  /** 显式切到演示数据 —— 只有这个入口会让人看到演示内容。 */
  showDemo: () => void;
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

/** 演示数据的会话信息（只在演示模式下使用，绝不冒充真实会话）。 */
export const DEMO_LEASE = { holder: "ai:demo-agent", epoch: 1 };

/** Tauri 是否在场（浏览器里跑 `pnpm dev` 时不在）。 */
function tauriAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** URL 上显式要求演示数据？(`?demo=1`) —— 这是"知情"的入口之一。 */
function demoRequestedFromUrl(): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return new URLSearchParams(window.location.search).get("demo") === "1";
}

/**
 * 取载荷。
 *
 * @param initialPath 启动时要打开的轨迹路径（为空则是空状态，不是演示数据）
 */
export function useViewPayload(initialPath = ""): ViewPayloadResult {
  const [demoRequested, setDemoRequested] = useState<boolean>(() => demoRequestedFromUrl());
  const [state, setState] = useState<WorkbenchState>(() =>
    // 初值就是空状态：**不要**在还没判断之前先拿演示数据垫一下。
    buildWorkbenchState(emptyPayload("还没有加载轨迹"), { lease: { holder: "", epoch: 0 } }),
  );
  const [source, setSource] = useState<PayloadSource>("loading");

  const load = useCallback(
    (path: string, demo: boolean) => {
      const plan = resolvePayloadPlan({
        tauriAvailable: tauriAvailable(),
        path,
        demoRequested: demo,
      });

      if (plan.kind === "demo") {
        // 演示数据：**在模型里留下一条告警**，界面会把它渲染在最上面（与真实告警同位置）。
        setState(
          buildWorkbenchState(
            {
              ...demoPayload(),
              warnings: [`当前显示的是**演示数据**（${plan.reason}）`],
            },
            { lease: DEMO_LEASE },
          ),
        );
        setSource("demo");
        return;
      }

      if (plan.kind === "empty") {
        setState(
          buildWorkbenchState(emptyPayload(plan.reason), { lease: { holder: "", epoch: 0 } }),
        );
        setSource("empty");
        return;
      }

      void (async () => {
        try {
          // 动态导入：不在 Tauri 里跑时不该因为缺少这个包而整个界面挂掉。
          const { invoke } = await import("@tauri-apps/api/core");
          const payload = (await invoke("view_trace", { path: plan.path, head: 200 })) as
            | object
            | string;
          const parsed = typeof payload === "string" ? JSON.parse(payload) : payload;
          setState(buildWorkbenchState(parsed));
          setSource("tauri");
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          // 失败就说失败：**绝不**退回演示数据（那会让"读不到文件"看起来像"有数据"）。
          setState(
            buildWorkbenchState(emptyPayload(`读取轨迹失败：${message}`), {
              lease: { holder: "", epoch: 0 },
            }),
          );
          setSource("empty");
        }
      })();
    },
    [],
  );

  const reload = useCallback((path?: string) => load(path ?? initialPath, false), [load, initialPath]);
  const showDemo = useCallback(() => {
    setDemoRequested(true);
    load("", true);
  }, [load]);

  useEffect(() => {
    load(initialPath, demoRequested);
  }, [load, initialPath, demoRequested]);

  return { state, source, reload, showDemo };
}
