/**
 * `ndjson.mjs` 的类型声明。
 *
 * 实现是纯 ESM（这样它能在**没有任何依赖**的情况下被 `node --test` 验证 ——
 * 本机装不上 Node 依赖）。类型不改变行为，行为由 `ndjson.test.mjs` 钉住。
 */

import type { AuditEntry, CapabilitySet, EventView, SessionSummary } from "./hub";

/** 传输层的最小接口。真实的回环 TCP 与测试里的假实现都满足它。 */
export interface NdjsonTransport {
  send(text: string): void;
  onLine(callback: (line: string) => void): void;
  close(): void;
}

export interface QueryEventsResult {
  items: EventView[];
  next: number;
  /** **必须常驻可见**：非 0 即代表这份结论建立在丢过数据的基础上。 */
  dropped: number;
}

export interface NdjsonClient {
  listSessions(): Promise<SessionSummary[]>;
  queryEvents(
    session: number,
    from: number,
    limit: number,
    foldRepeats: boolean,
  ): Promise<QueryEventsResult>;
  provenance(session: number): Promise<AuditEntry[]>;
  capabilities(): Promise<CapabilitySet>;
  takeover(session: number): Promise<number>;
  grantCapabilities(
    session: number,
    target: string,
    capabilities: string[],
  ): Promise<CapabilitySet>;
  close(): void;
  /** 在途请求数（诊断用）。 */
  inFlight(): number;
}

/** 校验目标主机是回环地址（非回环一律抛异常）。 */
export function assertLoopback(host: string): string;

/** 把 chunk 追加进缓冲区并切出完整行。 */
export function takeLines(state: { buffer: string }, chunk: string): string[];

export function createNdjsonClient(options: {
  transport: NdjsonTransport;
  timeoutMs?: number;
}): NdjsonClient;
