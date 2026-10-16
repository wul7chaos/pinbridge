/**
 * Hub 的 NDJSON 客户端（真实实现）。
 *
 * 为什么用 `.mjs` 而不是直接写 `.ts`：本机装不上 Node 依赖（registry 不可达），
 * 而**"改了但没验证"是这个项目一直在避免的东西**。写成纯 ESM 之后逻辑可以用
 * Node 自带的 `node --test` 跑用例验证（不需要任何依赖），TypeScript 侧只需要
 * 一份声明文件描述它 —— 类型不改变行为，行为已经被用例钉住了。
 *
 * 协议：本机回环 TCP 上的 NDJSON（见 docs/design/ipc-protocol.md）。
 * **只允许回环**：把调试能力暴露到网络上不是可配置项（同 pinbridge-client 的约束）。
 */

/** 允许的主机名：回环的三种写法。 */
const LOOPBACK_HOSTS = new Set(["127.0.0.1", "::1", "localhost"]);

/**
 * 校验目标主机是回环地址。
 *
 * 这个检查放在客户端里、而不是交给调用方自觉：**安全约束不能是"记得加上"**。
 * @param {string} host
 * @returns {string} 原样返回（便于链式使用）
 */
export function assertLoopback(host) {
  if (typeof host !== "string" || !LOOPBACK_HOSTS.has(host)) {
    throw new Error(
      `拒绝连接非回环地址 ${JSON.stringify(host)}：调试能力只允许通过回环暴露（见安全模型）`,
    );
  }
  return host;
}

/**
 * 把一个 chunk 追加到缓冲区并切出完整行。
 *
 * 分帧必须自己处理：TCP 不保证"一次 recv 就是一条消息"，**也不保证一条消息只出现
 * 在一个 chunk 里**。把 chunk 当消息是这类客户端最常见的错误 —— 平时看不出问题，
 * 忙起来就开始丢事件。
 *
 * @param {{buffer: string}} state
 * @param {string} chunk
 * @returns {string[]} 完整行（不含换行符）
 */
export function takeLines(state, chunk) {
  state.buffer += chunk;
  const lines = [];
  let index = state.buffer.indexOf("\n");
  while (index >= 0) {
    const line = state.buffer.slice(0, index).trim();
    state.buffer = state.buffer.slice(index + 1);
    // 空行是合法的保活/分隔，忽略而不是当成解析失败。
    if (line !== "") {
      lines.push(line);
    }
    index = state.buffer.indexOf("\n");
  }
  return lines;
}

/**
 * 创建客户端。
 *
 * `transport` 是一个最小接口：`{ send(text), onLine(cb), close() }`。
 * 真实的回环 TCP 实现与测试里的假实现都满足它 —— 这样"分帧、关联、错误传播"
 * 这些容易写错的部分可以在没有网络的情况下被完整验证。
 *
 * @param {{transport: {send: (text: string) => void, onLine: (cb: (line: string) => void) => void, close: () => void}, timeoutMs?: number}} options
 */
export function createNdjsonClient({ transport, timeoutMs = 5000 }) {
  const pending = new Map();
  let nextId = 1;

  transport.onLine((line) => {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      // 解析失败**不能吞掉**：它说明协议层已经出问题，静默忽略只会让上层
      // 一直等一个永远不会到的响应。
      for (const [, entry] of pending) {
        entry.reject(new Error(`收到无法解析的帧: ${line.slice(0, 200)}`));
      }
      pending.clear();
      return;
    }
    const entry = pending.get(frame.id);
    if (entry === undefined) {
      return; // 不属于任何在途请求（可能是服务端的通知）
    }
    pending.delete(frame.id);
    if (frame.error !== undefined) {
      entry.reject(new Error(String(frame.error)));
      return;
    }
    entry.resolve(frame.result);
  });

  /**
   * 发一个请求并等它的结果。
   * @param {string} method
   * @param {unknown[]} params
   */
  function call(method, params) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`${method} 超时（${timeoutMs}ms）`));
      }, timeoutMs);
      pending.set(id, {
        resolve: (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      transport.send(`${JSON.stringify({ id, method, params })}\n`);
    });
  }

  return {
    /** 会话列表。 */
    listSessions: () => call("list_sessions", []),

    /**
     * 取事件。
     *
     * **`dropped` 缺失就报错**，不给默认值：界面必须常驻显示"这份结论建立在丢过
     * 数据的基础上"。给它一个默认 0 等于把这个信息伪造成"没丢"。
     */
    async queryEvents(session, from, limit, foldRepeats) {
      const payload = await call("query_events", [session, from, limit, foldRepeats]);
      if (payload === null || typeof payload !== "object" || !("dropped" in payload)) {
        throw new Error("query_events 的响应缺少 dropped 字段：不能假装没有丢过数据");
      }
      return payload;
    },

    /** 溯源（审计链）。 */
    provenance: (session) => call("provenance", [session]),

    /** 能力集。 */
    capabilities: () => call("capabilities", []),

    /** 人类接管：收回租约并推进 epoch。 */
    takeover: (session) => call("takeover", [session]),

    /** 授予能力（仅人类可用，见权限模型）。 */
    grantCapabilities: (session, target, capabilities) =>
      call("grant_capabilities", [session, target, capabilities]),

    /** 关掉传输层（退出时用）。 */
    close: () => transport.close(),

    /** 在途请求数（诊断用）。 */
    inFlight: () => pending.size,
  };
}
