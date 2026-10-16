/**
 * 事件流。
 *
 * 两个刻意的选择：
 *   1. **折叠记录原样显示**（`repeat` 行带 `×N`），而不是默认展开成大列表。
 *      展开 6 条还好，展开 4000 条会把真正有信息量的那几行冲走；
 *   2. `following` 由父组件控制。分析旧数据时必须能关掉自动滚动，
 *      否则一有新事件就把你正在看的那行顶走。
 */
import type { EventView } from "../api/hub";

export interface EventStreamProps {
  events: EventView[];
  following: boolean;
}

function hex(value: number): string {
  return `0x${value.toString(16).toUpperCase()}`;
}

export function EventStream({ events, following }: EventStreamProps) {
  return (
    <div className="event-stream">
      <div className="event-stream__head">
        <span>序号</span>
        <span>类型</span>
        <span>线程</span>
        <span>地址</span>
        <span>参数</span>
      </div>
      <ol className="event-stream__list" data-following={following}>
        {events.map((event) => (
          <li key={event.seq} className={`event event--${event.kind}`}>
            <span className="event__seq">#{event.seq}</span>
            <span className="event__kind">
              {event.kind}
              {event.repeats && event.repeats > 1 ? <em className="event__repeats"> ×{event.repeats}</em> : null}
            </span>
            <span className="event__thread">{event.thread}</span>
            <span className="event__addr">{hex(event.address)}</span>
            <span className="event__args">[{event.args.map(hex).join(", ")}]</span>
          </li>
        ))}
      </ol>
    </div>
  );
}
