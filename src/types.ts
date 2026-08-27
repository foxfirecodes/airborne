export type RuleKind = "cursor_bugbot_completed" | "buildkite_job_completed";
export type RuleState = "waiting" | "in_progress" | "passed" | "failed" | "unavailable" | "completed";

export interface Rule {
  id: string; kind: RuleKind; enabled: boolean; state: RuleState; source_url?: string | null;
  error?: string | null; job_name?: string | null; notify_on?: "terminal" | "passed"; config: Record<string, unknown>;
}
export interface Watch {
  id: string; url: string; title: string; head_sha: string; active: boolean; last_polled_at?: string | null; rules: Rule[];
}
export interface Alert { id: string; rule_id: string; watch_id: string; title: string; body: string; status: "unread" | "read"; created_at: string; source_url?: string | null; }
export interface PipelineMapping { github_status_context: string; organization: string; pipeline: string; available_job_names: string[]; }
export interface Settings { githubTokenConfigured: boolean; buildkiteTokenConfigured: boolean; githubToken?: string; buildkiteToken?: string; pollIntervalSeconds: number; pipelineMappings: PipelineMapping[]; }
export interface DashboardData { watches: Watch[]; alerts: Alert[]; last_poll_error?: string | null; last_poll_at?: string | null; }
export interface RuleInput { kind: RuleKind; job_name?: string; notify_on?: "terminal" | "passed"; github_status_context?: string; organization?: string; pipeline?: string; enabled?: boolean; }
