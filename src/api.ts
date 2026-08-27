import { invoke } from "@tauri-apps/api/core";
import type { Alert, DashboardData, Rule, RuleInput, Settings, Watch } from "./types";

const call = <T>(command: string, args?: Record<string, unknown>) => invoke<T>(command, args);
export const api = {
  dashboard: async () => normalizeDashboard(await call<RawDashboard>("get_dashboard")),
  addWatch: (url: string) => call<void>("add_watch", { url }),
  createRule: (watchId: string, input: RuleInput) => call<void>("create_rule", { watchId: Number(watchId), input: { kind: input.kind, config: ruleConfig(input), enabled: input.enabled ?? true } }),
  updateRule: (rule: Rule, input: Partial<RuleInput>) => call<void>("update_rule", { ruleId: Number(rule.id), input: { config: ruleConfig({ kind: rule.kind, job_name: rule.job_name ?? undefined, notify_on: rule.notify_on, github_status_context: typeof rule.config.github_status_context === "string" ? rule.config.github_status_context : undefined, organization: typeof rule.config.organization === "string" ? rule.config.organization : undefined, pipeline: typeof rule.config.pipeline === "string" ? rule.config.pipeline : undefined, ...input }), enabled: input.enabled ?? rule.enabled } }),
  deleteRule: (ruleId: string) => call<void>("delete_rule", { ruleId: Number(ruleId) }),
  refresh: () => call<void>("refresh_now"),
  markAlertRead: (alertId: string) => call<void>("mark_alert_read", { alertId: Number(alertId) }),
  markAllRead: () => call<void>("mark_all_alerts_read"),
  settings: async () => {
    const value = await call<RawSettings>("get_settings");
    const mappings = value.pipeline_mappings ?? value.pipelineMappings ?? [];
    return {
      githubTokenConfigured: value.github_token_configured ?? value.githubTokenConfigured ?? false,
      buildkiteTokenConfigured: value.buildkite_token_configured ?? value.buildkiteTokenConfigured ?? false,
      pollIntervalSeconds: value.poll_interval_seconds ?? value.pollIntervalSeconds ?? 60,
      pipelineMappings: mappings.map(item => ({ github_status_context: item.github_status_context ?? item.githubStatusContext ?? "", organization: item.organization, pipeline: item.pipeline, available_job_names: item.available_job_names ?? item.availableJobNames ?? [] })),
    };
  },
  saveSettings: (settings: Settings) => call<void>("save_settings", { settings: { pollIntervalSeconds: settings.pollIntervalSeconds, pipelineMappings: settings.pipelineMappings.map(item => ({ githubStatusContext: item.github_status_context, organization: item.organization, pipeline: item.pipeline, availableJobNames: item.available_job_names })), githubToken: settings.githubToken, buildkiteToken: settings.buildkiteToken } }),
  openUrl: (url: string) => call<void>("open_url", { url }),
};

type RawSettings = {
  poll_interval_seconds?: number; pollIntervalSeconds?: number;
  pipeline_mappings?: RawPipelineMapping[]; pipelineMappings?: RawPipelineMapping[];
  github_token_configured?: boolean; githubTokenConfigured?: boolean;
  buildkite_token_configured?: boolean; buildkiteTokenConfigured?: boolean;
};
type RawPipelineMapping = { github_status_context?: string; githubStatusContext?: string; organization: string; pipeline: string; available_job_names?: string[]; availableJobNames?: string[] };

type RawDashboard = { watches: Array<{ id: number; github_owner: string; github_repo: string; github_pr_number: number; title: string; active: boolean; head_sha?: string; last_poll_at?: string; last_error?: string }>; rules: Array<{ id: number; watch_id: number; kind: Rule["kind"]; enabled: boolean; config_json: string }>; observations: Array<{ rule_id: number; head_sha: string; state: Rule["state"]; source_url?: string; detail?: string; observed_at: string }>; alerts: Array<{ id: number; rule_id: number; head_sha: string; title: string; body: string; status: Alert["status"]; created_at: string }>; };
function parseConfig(value: string) { try { return JSON.parse(value) as Record<string, unknown>; } catch { return {}; } }
function normalizeDashboard(raw: RawDashboard): DashboardData {
  const rules = raw.rules.map(rule => { const config = parseConfig(rule.config_json); const observation = raw.observations.filter(item => item.rule_id === rule.id).sort((a, b) => b.observed_at.localeCompare(a.observed_at))[0]; return { id: String(rule.id), kind: rule.kind, enabled: rule.enabled, config, state: observation?.state ?? "waiting", source_url: observation?.source_url, error: observation?.detail, job_name: typeof config.job_name === "string" ? config.job_name : undefined, notify_on: config.notify_on === "passed" ? "passed" : "terminal" } satisfies Rule; });
  const watchByRule = new Map(raw.rules.map(item => [item.id, item.watch_id]));
  const alerts: Alert[] = raw.alerts.map(alert => ({ ...alert, id: String(alert.id), rule_id: String(alert.rule_id), watch_id: String(watchByRule.get(alert.rule_id) ?? ""), source_url: raw.observations.find(observation => observation.rule_id === alert.rule_id && observation.head_sha === alert.head_sha)?.source_url }));
  const watches: Watch[] = raw.watches.map(watch => ({ id: String(watch.id), url: `https://github.com/${watch.github_owner}/${watch.github_repo}/pull/${watch.github_pr_number}`, title: watch.title, head_sha: watch.head_sha ?? "", active: watch.active, last_polled_at: watch.last_poll_at, rules: rules.filter(rule => raw.rules.find(rawRule => rawRule.id === Number(rule.id))?.watch_id === watch.id) }));
  const pollTimes = raw.watches.map(watch => watch.last_poll_at).filter((time): time is string => Boolean(time)).sort();
  return { watches, alerts, last_poll_error: raw.watches.find(watch => watch.last_error)?.last_error, last_poll_at: pollTimes[pollTimes.length - 1] };
}
function ruleConfig(input: RuleInput): Record<string, unknown> { return input.kind === "cursor_bugbot_completed" ? { check_name: "Cursor Bugbot" } : { github_status_context: input.github_status_context, organization: input.organization, pipeline: input.pipeline, job_name: input.job_name, notify_on: input.notify_on ?? "terminal" }; }
