/**
 * NDJSON 客户端的用例（`node --test`，**不需要任何依赖**）。
 *
 * 为什么这些用例值得写：分帧、请求关联、错误传播是这类客户端最容易写错、
 * 而且**平时看不出问题**的三件事。等到"忙起来开始丢事件"时，症状会离原因很远。
 */

import assert from "node:assert/strict";
import test from "node:test";

import { assertLoopback, createNdjsonClient, takeLines } from "./ndjson.mjs";

/** 测试用的假传输：手动喂 chunk、记录发出去的内容。 */
function fakeTransport() {
  let onLine = () => {};
  const sent = [];
  return {
    sent,
    send(text) {
      sent.push(text);
    },
    onLine(callback) {
      onLine = callback;
    },
    /** 把一个 chunk 喂进来（可以只是半条消息）。 */
    feed(chunk) {
      const state = this._state ?? (this._state = { buffer: "" });
      for (const line of takeLines(state, chunk)) {
        onLine(line);
      }
    },
    close() {
      this.closed = true;
    },
  };
}

test("半条消息不会触发任何回调，补齐后才触发", async () => {
  const transport = fakeTransport();
  const client = createNdjsonClient({ transport });
  // 先发请求，再分两次喂响应 —— TCP 不保证"一次 recv 就是一条消息"。
  const pending = client.listSessions();
  const request = JSON.parse(transport.sent[0]);
  transport.feed(`${JSON.stringify({ id: request.id, result: { ok: true } }).slice(0, 12)}`);
  assert.equal(client.inFlight(), 1, "请求已发出，仍在等待");
  transport.feed(`${JSON.stringify({ id: request.id, result: { ok: true } }).slice(12)}\n`);
  assert.deepEqual(await pending, { ok: true });
  assert.equal(client.inFlight(), 0);
});

test("一个 chunk 里的多条消息会依次被处理", async () => {
  const transport = fakeTransport();
  const client = createNdjsonClient({ transport });
  const first = client.listSessions();
  const second = client.capabilities();
  assert.equal(client.inFlight(), 2);
  const request = JSON.parse(transport.sent[0]);
  const request2 = JSON.parse(transport.sent[1]);
  // 两条响应挤在同一个 chunk 里（真实网络下很常见）
  transport.feed(
    `${JSON.stringify({ id: request.id, result: { sessions: [] } })}\n` +
      `${JSON.stringify({ id: request2.id, result: { bits: 257 } })}\n`,
  );
  assert.deepEqual(await first, { sessions: [] });
  assert.deepEqual(await second, { bits: 257 });
});

test("错误帧会以异常形式抛给调用方，而不是被当成结果", async () => {
  const transport = fakeTransport();
  const client = createNdjsonClient({ transport });
  const pending = client.takeover(7);
  const request = JSON.parse(transport.sent[0]);
  transport.feed(`${JSON.stringify({ id: request.id, error: "能力不足：capability_grant" })}\n`);
  await assert.rejects(pending, /能力不足/);
});

test("无法解析的帧必须让所有在途请求失败，而不是让它们永远等下去", async () => {
  const transport = fakeTransport();
  const client = createNdjsonClient({ transport });
  const pending = client.capabilities();
  transport.feed("这不是 JSON\n");
  await assert.rejects(pending, /无法解析/);
  assert.equal(client.inFlight(), 0);
});

test("queryEvents 缺少 dropped 字段时必须报错（不能伪造成没丢数据）", async () => {
  const transport = fakeTransport();
  const client = createNdjsonClient({ transport });
  const pending = client.queryEvents(1, 0, 10, true);
  const request = JSON.parse(transport.sent[0]);
  transport.feed(`${JSON.stringify({ id: request.id, result: { items: [], next: 0 } })}\n`);
  await assert.rejects(pending, /缺少 dropped/);

  const ok = client.queryEvents(1, 0, 10, true);
  const request2 = JSON.parse(transport.sent[1]);
  transport.feed(
    `${JSON.stringify({ id: request2.id, result: { items: [], next: 0, dropped: 12 } })}\n`,
  );
  assert.equal((await ok).dropped, 12, "dropped 要原样交给界面常驻显示");
});

test("非回环地址一律拒绝（安全约束不能靠调用方自觉）", () => {
  assert.equal(assertLoopback("127.0.0.1"), "127.0.0.1");
  assert.equal(assertLoopback("::1"), "::1");
  assert.equal(assertLoopback("localhost"), "localhost");
  for (const host of ["0.0.0.0", "192.168.1.10", "example.com", "", undefined]) {
    assert.throws(() => assertLoopback(host), /非回环/, `必须拒绝 ${JSON.stringify(host)}`);
  }
});

test("超时会拒绝，而不是让调用方无限等待", async () => {
  const transport = fakeTransport();
  const client = createNdjsonClient({ transport, timeoutMs: 20 });
  await assert.rejects(client.capabilities(), /超时/);
  assert.equal(client.inFlight(), 0, "超时后不能留下悬挂的请求");
});
