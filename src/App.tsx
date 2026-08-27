import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import { Dashboard } from "./pages/Dashboard";
import { Settings } from "./pages/Settings";
import type { DashboardData, RuleInput, Settings as SettingsType } from "./types";

const empty: DashboardData = { watches: [], alerts: [] };
export default function App() {
  const [page, setPage] = useState<"dashboard" | "settings">("dashboard"); const [data, setData] = useState<DashboardData>(empty); const [settings, setSettings] = useState<SettingsType | null>(null); const [error, setError] = useState<string | null>(null); const [refreshing, setRefreshing] = useState(false);
  const load = useCallback(async () => { try { setError(null); setData(await api.dashboard()); } catch (e) { setError(String(e)); } }, []);
  const loadSettings = useCallback(async () => { try { setSettings(await api.settings()); } catch (e) { setError(String(e)); } }, []);
  useEffect(() => { void load(); void loadSettings(); }, [load, loadSettings]);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen("dashboard-changed", () => { void load(); }).then(stop => { unlisten = stop; });
    return () => unlisten?.();
  }, [load]);
  async function change(action: () => Promise<void>) { try { setError(null); await action(); await load(); } catch (e) { setError(String(e)); } }
  async function refresh() { setRefreshing(true); await change(api.refresh); setRefreshing(false); }
  return <div className="app-shell"><aside><div className="brand"><span>◒</span><strong>PR Watcher</strong></div><nav><button className={page === "dashboard" ? "active" : ""} onClick={() => setPage("dashboard")}>Overview</button><button className={page === "settings" ? "active" : ""} onClick={() => setPage("settings")}>Settings</button></nav><p className="sidebar-note">Watches run in the menu bar, even when this window is closed.</p></aside><div className="content">{error && <div className="app-error"><strong>Couldn’t talk to the app backend.</strong><span>{error}</span><button onClick={() => { void load(); void loadSettings(); }}>Try again</button></div>}{page === "dashboard" ? <Dashboard data={data} mappings={settings?.pipelineMappings || []} refreshing={refreshing} onRefresh={() => void refresh()} onAddWatch={url => change(() => api.addWatch(url))} onCreateRule={(id, input) => change(() => api.createRule(id, input))} onUpdateRule={(rule, input) => change(() => api.updateRule(rule, input))} onDeleteRule={id => change(() => api.deleteRule(id))} onRead={id => void change(() => api.markAlertRead(id))} onReadAll={() => void change(api.markAllRead)} onOpen={url => void api.openUrl(url)} /> : <Settings initial={settings} onSave={async next => { await change(() => api.saveSettings(next)); await loadSettings(); }} />}</div></div>;
}
