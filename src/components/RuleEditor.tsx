import { useState } from "react";
import type { PipelineMapping, RuleInput } from "../types";

export function RuleEditor({ mappings, initial, onSubmit, onClose, submitLabel = "Add rule" }: { mappings: PipelineMapping[]; initial?: RuleInput; onSubmit: (input: RuleInput) => Promise<void>; onClose: () => void; submitLabel?: string }) {
  const [kind, setKind] = useState<RuleInput["kind"]>(initial?.kind ?? "cursor_bugbot_completed");
  const [mappingIndex, setMappingIndex] = useState(() => Math.max(0, mappings.findIndex(mapping => mapping.github_status_context === initial?.github_status_context && mapping.organization === initial?.organization && mapping.pipeline === initial?.pipeline)));
  const [jobName, setJobName] = useState(initial?.job_name ?? "");
  const [notifyOn, setNotifyOn] = useState<"terminal" | "passed">(initial?.notify_on ?? "terminal");
  const [busy, setBusy] = useState(false);
  const mapping = mappings[mappingIndex] ?? { github_status_context: initial?.github_status_context ?? "", organization: initial?.organization ?? "", pipeline: initial?.pipeline ?? "", available_job_names: [] };
  async function submit(e: React.FormEvent) {
    e.preventDefault(); setBusy(true);
    try {
      await onSubmit(kind === "cursor_bugbot_completed" ? { kind } : { kind, job_name: jobName, notify_on: notifyOn, github_status_context: mapping?.github_status_context, organization: mapping?.organization, pipeline: mapping?.pipeline });
      onClose();
    } finally { setBusy(false); }
  }
  return <form className="rule-editor" onSubmit={submit}>
    <label>Watch for<select value={kind} onChange={e => setKind(e.target.value as RuleInput["kind"])}><option value="cursor_bugbot_completed">Cursor Bugbot completion</option><option value="buildkite_job_completed">Buildkite job completion</option></select></label>
    {kind === "buildkite_job_completed" && <>
      {mappings.length > 0 && <label>Pipeline<select value={mappingIndex} onChange={e => setMappingIndex(Number(e.target.value))}>{mappings.map((item, index) => <option key={item.github_status_context} value={index}>{item.organization}/{item.pipeline}</option>)}</select></label>}
      <label>Job name<input list="job-names" required value={jobName} onChange={e => setJobName(e.target.value)} placeholder="Exact Buildkite job name" /><datalist id="job-names">{mapping?.available_job_names.map(name => <option key={name} value={name} />)}</datalist></label>
      <label>Notify when<select value={notifyOn} onChange={e => setNotifyOn(e.target.value as "terminal" | "passed")}><option value="terminal">Finished (pass or fail)</option><option value="passed">Passed only</option></select></label>
    </>}
    <div className="form-actions"><button type="button" className="secondary" onClick={onClose}>Cancel</button><button disabled={busy}>{busy ? "Saving…" : submitLabel}</button></div>
  </form>;
}
