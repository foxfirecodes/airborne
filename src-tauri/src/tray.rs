/// UI-independent tray state. Platform code uses this to decide whether to show
/// the attention image every 600ms; keeping it pure makes unread semantics testable.
#[derive(Debug, Default)]
pub struct TrayState {
    unread: usize,
    attention: bool,
}

pub fn normal_icon() -> tauri::image::Image<'static> {
    icon([40, 44, 52, 255])
}
pub fn attention_icon() -> tauri::image::Image<'static> {
    icon([232, 80, 93, 255])
}
fn icon(color: [u8; 4]) -> tauri::image::Image<'static> {
    tauri::image::Image::new_owned(
        color.into_iter().cycle().take(16 * 16 * 4).collect(),
        16,
        16,
    )
}
impl TrayState {
    pub fn set_unread(&mut self, count: usize) {
        self.unread = count;
        if count == 0 {
            self.attention = false
        }
    }
    pub fn tick(&mut self) -> bool {
        if self.unread > 0 {
            self.attention = !self.attention
        };
        self.attention
    }
    pub fn blinking(&self) -> bool {
        self.unread > 0
    }
    pub fn attention_icon(&self) -> bool {
        self.attention
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stops_when_last_alert_read() {
        let mut t = TrayState::default();
        t.set_unread(1);
        assert!(t.blinking());
        assert!(t.tick());
        t.set_unread(0);
        assert!(!t.blinking());
        assert!(!t.attention_icon());
    }
}
