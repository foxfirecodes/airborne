use crate::models::Alert;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;
/// Kept separate so desktop notification is an effect of an inserted alert, never evaluation.
pub trait AlertSink: Send + Sync {
    fn notify(&self, alert: &Alert);
    fn dashboard_changed(&self) {}
}
pub struct NoopAlertSink;
impl AlertSink for NoopAlertSink {
    fn notify(&self, _: &Alert) {}
}

/// The only place a desktop notification is emitted. The poller invokes this
/// only after the alert row was inserted, so a retry can never notify twice.
pub struct NativeAlertSink {
    pub app: AppHandle,
}
impl AlertSink for NativeAlertSink {
    fn notify(&self, alert: &Alert) {
        let _ = self
            .app
            .notification()
            .builder()
            .title(&alert.title)
            .body(&alert.body)
            .show();
        self.dashboard_changed();
    }
    fn dashboard_changed(&self) {
        let _ = self.app.emit("dashboard-changed", ());
    }
}
