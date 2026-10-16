/**
 * 会话栏：把"这份数据能不能用"的三件事摆在最上面。
 *
 * 刻意不做折叠、不做二级菜单——使用者在分析中途只需要扫一眼，
 * 就应该知道：现在谁在驾驶、录到哪了、丢没丢数据。
 */
import type { SessionSummary, TraceSummary } from "../api/hub";
import { foldRatio } from "../api/hub";

export interface SessionBarProps {
  session: SessionSummary;
  summary: TraceSummary;
  onTakeover: (session: number) => void;
}

export function SessionBar({ session, summary, onTakeover }: SessionBarProps) {
  const lossy = summary.dropped > 0 || summary.truncated;
  // 驾驶者用 ai:/human: 前缀区分；这里直接展示审计名，避免出现两套叫法
  const pilot = session.lease_holder.startsWith("ai:") ? "AI" : "人类";

  return (
    <header className="session-bar">
      <div className="session-bar__row">
        <strong>
          会话 {session.session} · {session.image_path}
        </strong>
        <span className="tag">pid {session.pid}</span>
        <span className="tag">{session.state}</span>
        <span className="tag">{session.scripts} 个脚本</span>
        <span className={`tag ${pilot === "AI" ? "tag--ai" : "tag--human"}`}>
          {pilot} 驾驶（{session.lease_holder}）
        </span>
        <button type="button" onClick={() => onTakeover(session.session)}>
          接管
        </button>
      </div>

      <div className="session-bar__row session-bar__row--metrics">
        <span>记录 {summary.records}</span>
        <span>事件 {summary.events}</span>
        <span>折叠率 {(foldRatio(summary) * 100).toFixed(2)}%</span>
        <span className={lossy ? "metric--warn" : "metric--ok"}>
          {lossy
            ? `不完整：丢弃 ${summary.dropped}${summary.truncated ? " + 被中断" : ""}`
            : "轨迹完整（无丢弃、未截断）"}
        </span>
      </div>
    </header>
  );
}
