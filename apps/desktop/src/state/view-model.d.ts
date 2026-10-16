/** `view-model.mjs` 的类型声明（实现是零依赖 ESM，逻辑由 view-model.test.mjs 钉住）。 */

export const SECTION_ORDER: string[];

export interface DroppedView {
  text: string;
  critical: boolean;
}

export interface LeaseView {
  text: string;
  holder: string;
  epoch: number;
  isHuman: boolean;
}

export interface CoverageView {
  capped: boolean;
  sites: number;
  skipped: number;
  unreadable: number;
  filtered: number;
  text: string;
}

export interface EventRow {
  seq: number;
  kind: string;
  thread: number;
  address: string;
  repeats: string;
  args: string[];
}

export interface KindRow {
  kind: string;
  count: number;
}

export interface WorkbenchStatus {
  records: number;
  events: number;
  folded: number;
  foldRatio: number;
  dropped: DroppedView;
  /** 原始计数（不要让界面自己凑数字）。 */
  droppedRaw: number;
  summaryText: string | null;
}

export interface WorkbenchState {
  sections: string[];
  warnings: string[];
  status: WorkbenchStatus | null;
  lease: LeaseView;
  coverage: CoverageView | null;
  events: EventRow[];
  kinds: KindRow[];
  records: number;
  takeoverEnabled: boolean;
}

export function formatAddress(value: number): string;
export function formatRepeats(repeats: number | undefined): string;
export function formatDropped(dropped: number): DroppedView;
export function formatLease(lease: { holder?: string; epoch?: number } | undefined): LeaseView;
export function buildWorkbenchState(
  payload: object | null,
  options?: { lease?: { holder?: string; epoch?: number }; takeoverEnabled?: boolean },
): WorkbenchState;
