import type { Rule, Watch } from "../types";
import { RuleEditor } from "./RuleEditor";
import type { PipelineMapping, RuleInput } from "../types";
import { useState } from "react";

const names: Record<Rule["state"], string> = { waiting: "Waiting", in_progress: "In progress", passed: "Passed", failed: "Failed", unavailable: "Unavailable", completed: "Completed" };
function buildUrl(jobUrl?: string | null) {
  if (!jobUrl) return undefined;
  try {
    const url = new URL(jobUrl);
    if (url.hostname !== "buildkite.com") return undefined;
    url.hash = "";
    return url.toString();
  } catch { return undefined; }
}
export function WatchRow({ watch, mappings, onCreateRule, onUpdateRule, onDeleteRule, onOpen }: { watch: Watch; mappings: PipelineMapping[]; onCreateRule: (id: string, input: RuleInput) => Promise<void>; onUpdateRule: (rule: Rule, input: Partial<RuleInput>) => Promise<void>; onDeleteRule: (id: string) => Promise<void>; onOpen: (url: string) => void }) {
  const [expanded, setExpanded] = useState(false); const [adding, setAdding] = useState(false); const [editing, setEditing] = useState<Rule | null>(null);
  return <article className="watch"><div className="watch-summary"><button className="watch-main" onClick={() => setExpanded(!expanded)}><span className="chevron">{expanded ? "⌄" : "›"}</span><span><strong>{watch.title}</strong><small>{watch.head_sha.slice(0, 8)} · {watch.last_polled_at ? `Checked ${new Date(watch.last_polled_at).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}` : "Not checked yet"}</small></span></button><div className="badges">{watch.rules.map(rule => <span key={rule.id} className={`badge ${rule.state}`}>{names[rule.state]}</span>)}</div><button className="icon-button" aria-label="Open pull request" onClick={() => onOpen(watch.url)}>↗</button></div>
  {expanded && <div className="watch-detail"><div className="rules">{watch.rules.length === 0 && <p className="muted">No rules yet.</p>}{watch.rules.map(rule => <div className="rule" key={rule.id}><div><strong>{rule.kind === "cursor_bugbot_completed" ? "Cursor Bugbot" : rule.job_name || "Buildkite job"}</strong><p>{rule.error || names[rule.state]}{buildUrl(rule.source_url) && <button className="link" onClick={() => onOpen(buildUrl(rule.source_url)!)}>Open build ↗</button>}{rule.source_url && <button className="link" onClick={() => onOpen(rule.source_url!)}>{buildUrl(rule.source_url) ? "Open job ↗" : "Open source ↗"}</button>}</p></div><div><button className="secondary" onClick={() => setEditing(rule)}>Edit</button><button className="secondary" onClick={() => onUpdateRule(rule, { enabled: !rule.enabled })}>{rule.enabled ? "Pause" : "Resume"}</button><button className="danger" onClick={() => { if (confirm("Delete this rule?")) void onDeleteRule(rule.id); }}>Delete</button></div></div>)}</div>{editing && <RuleEditor key={editing.id} mappings={mappings} initial={{ kind: editing.kind, job_name: editing.job_name ?? undefined, notify_on: editing.notify_on, github_status_context: typeof editing.config.github_status_context === "string" ? editing.config.github_status_context : undefined, organization: typeof editing.config.organization === "string" ? editing.config.organization : undefined, pipeline: typeof editing.config.pipeline === "string" ? editing.config.pipeline : undefined, enabled: editing.enabled }} onSubmit={async input => { await onUpdateRule(editing, input); setEditing(null); }} onClose={() => setEditing(null)} submitLabel="Save rule" />}{adding ? <RuleEditor mappings={mappings} onSubmit={input => onCreateRule(watch.id, input)} onClose={() => setAdding(false)} /> : <button className="secondary add-rule" onClick={() => setAdding(true)}>+ Add rule</button>}</div>}
  </article>;
}
