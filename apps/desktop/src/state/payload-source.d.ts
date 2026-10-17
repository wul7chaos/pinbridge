/** `payload-source.mjs` 的类型。实现是纯 JS（零依赖），这里只描述形状。 */

export type PayloadPlan =
  | { kind: "real"; path: string }
  | { kind: "demo"; reason: string }
  | { kind: "empty"; reason: string };

export function resolvePayloadPlan(input?: {
  tauriAvailable?: boolean;
  path?: string;
  demoRequested?: boolean;
}): PayloadPlan;

export function emptyPayload(reason: string): {
  path: string;
  summary: {
    records: number;
    events: number;
    folded: number;
    dropped: number;
    truncated: boolean;
  };
  kinds: unknown[];
  events: unknown[];
  records: number;
  has_more: boolean;
  warnings: string[];
};
