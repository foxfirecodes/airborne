import type { Alert } from "../types";

export function AlertList({ alerts, onRead, onOpen }: { alerts: Alert[]; onRead: (id: string) => void; onOpen: (url: string) => void }) {
  if (!alerts.length) return <p className="muted">Nothing needs your attention.</p>;
  return <div className="alerts">{alerts.map(alert => <article className={`alert ${alert.status}`} key={alert.id}><div><strong>{alert.title}</strong><p>{alert.body}</p><time>{new Date(alert.created_at).toLocaleString()}</time></div><div className="alert-actions">{alert.source_url && <button className="icon-button" aria-label="Open source" onClick={() => onOpen(alert.source_url!)}>↗</button>}{alert.status === "unread" && <button className="secondary" onClick={() => onRead(alert.id)}>Read</button>}</div></article>)}</div>;
}
