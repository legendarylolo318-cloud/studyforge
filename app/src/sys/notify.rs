//! Desktop notifications via notify-rust (fire-and-forget on a worker thread).

pub fn send(summary: &str, body: &str) {
    let (s, b) = (summary.to_string(), body.to_string());
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.appname("StudyForge").summary(&s).body(&b);
        if let Err(e) = n.show() {
            tracing::warn!("notification failed: {e}");
        }
    });
}
