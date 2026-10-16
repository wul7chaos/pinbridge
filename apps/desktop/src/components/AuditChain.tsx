/**
 * 审计链。
 *
 * **失败记录与成功记录同等重要**：AI 的失败尝试是理解它"为什么改变了策略"
 * 的唯一线索。所以这里对 `outcome !== "ok"` 的行做视觉强调，
 * 而不是把它降级成一行淡灰色的小字。
 */
import type { AuditEntry } from "../api/hub";

export interface AuditChainProps {
  entries: AuditEntry[];
}

export function AuditChain({ entries }: AuditChainProps) {
  return (
    <ol className="audit">
      {entries.map((entry) => {
        const failed = entry.outcome !== "ok";
        return (
          <li key={entry.seq} className={failed ? "audit__item audit__item--failed" : "audit__item"}>
            <div className="audit__head">
              <span className={`tag tag--${entry.actor.kind}`}>{entry.actor.kind}</span>
              <strong>{entry.action}</strong>
              <time>{entry.at}</time>
            </div>
            <div className="audit__detail">{entry.detail}</div>
            <div className="audit__outcome">{entry.outcome}</div>
          </li>
        );
      })}
    </ol>
  );
}
