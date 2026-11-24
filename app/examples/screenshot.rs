//! Headless UI renderer for development: renders each page with demo data
//! through Slint's software renderer into PNG files (no display required).
//!
//!     cargo run -p studyforge --example screenshot -- /tmp/shots [light]

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, ModelRc, PhysicalSize, VecModel};
use std::rc::Rc;

slint::include_modules!();

struct P(Rc<MinimalSoftwareWindow>);
impl Platform for P {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn bar(label: &str, value: f32, text: &str, color: slint::Color) -> BarItem {
    BarItem { label: label.into(), value, text: text.into(), color }
}

fn demo(ui: &AppWindow) {
    let st = ui.global::<AppState>();
    let c = |r, g, b| slint::Color::from_rgb_u8(r, g, b);
    st.set_status_line("Blocking helper not installed".into());
    st.set_subjects(model(vec![
        SubjectItem { id: 1, name: "Mathematics".into(), color: c(108, 140, 255) },
        SubjectItem { id: 2, name: "Biology".into(), color: c(47, 197, 160) },
    ]));
    st.set_subject_names(model(vec!["No subject".into(), "Mathematics".into(), "Biology".into()]));
    st.set_tips(model(vec![TipItem { id: 1, text: "Drink a glass of water.".into() }]));
    st.set_settings(SettingsData {
        work_mins: 25, short_break_mins: 5, long_break_mins: 15, cycles_before_long: 4, auto_start_next: true,
        flow_reminder_mins: 0, theme: 0, accent: 0, daily_goal_mins: 240, weekly_goal_mins: 1200, streak_min_mins: 25,
        day_start_hour: 4, new_cards_per_day: 20, master_volume: 0.8, sound_during_work: false, notify_sound: true,
        notifications: true, close_to_tray: true, autostart: false, hotkey: "Ctrl+Alt+S".into(), dnd_during_work: false,
        browser_policies: true, block_steam_client: false, hard_mode_cooldown_mins: 20, emergency_cooldown_mins: 60,
        early_exit_challenge: true, backup_daily: true,
    });
    st.set_hotkey_status("Hotkey active: Ctrl+Alt+S".into());
    // dashboard
    st.set_dash_today_text("2h 05m".into());
    st.set_dash_today_progress(0.52);
    st.set_dash_today_sub("2h 05m of 4h 00m".into());
    st.set_dash_week_text("9h 40m".into());
    st.set_dash_week_progress(0.48);
    st.set_dash_week_sub("9h 40m of 20h 00m".into());
    st.set_dash_streak(9);
    st.set_dash_freezes(1);
    st.set_dash_streak_note("Today counts ✓".into());
    st.set_dash_cards_due(34);
    let todo = |id, t: &str, s: &str, due: &str, p, overdue| TodoItem {
        id, title: t.into(), subject: s.into(), subject_color: c(108, 140, 255), due: due.into(), priority: p, done: false, overdue,
    };
    let todos = vec![
        todo(1, "Problem set 4", "Mathematics", "Nov 22", 2, true),
        todo(2, "Read chapter 7", "Biology", "Nov 26", 1, false),
        todo(3, "Lab report outline", "Biology", "Dec 2", 0, false),
    ];
    st.set_next_tasks(model(todos.clone()));
    st.set_todos(model(todos));
    st.set_stats_subject_bars(model(vec![bar("Mathematics", 1.0, "6h 10m", c(108, 140, 255)), bar("Biology", 0.55, "3h 20m", c(47, 197, 160))]));
    // focus
    st.set_timer_text("18:42".into());
    st.set_phase_label("Focus".into());
    st.set_timer_progress(0.25);
    st.set_timer_active(true);
    st.set_timer_running(true);
    st.set_session_info("Cycle 2 · 31:18 focused".into());
    st.set_active_goal("Finish integration exercises".into());
    st.set_tracks(model(vec![
        TrackItem { id: "white".into(), name: "White noise".into(), volume: 0.0 },
        TrackItem { id: "brown".into(), name: "Brown noise".into(), volume: 0.4 },
    ]));
    // notes
    st.set_notes(model(vec![
        NoteItem { id: 1, title: "Calculus notes".into(), snippet: "Derivatives of trig functions…".into(), subject: "Mathematics".into(), updated: "Nov 24 14:02".into(), selected: true },
        NoteItem { id: 2, title: "Cell biology".into(), snippet: "Mitochondria are the…".into(), subject: "Biology".into(), updated: "Nov 23 09:12".into(), selected: false },
    ]));
    st.set_current_note_id(1);
    st.set_note_title("Calculus notes".into());
    st.set_note_body("# Derivatives\n\nThe **power rule** says d/dx x^n = n x^(n-1).\n\n- sin → cos\n- cos → -sin\n\n```\nf(x) = x^2\n```".into());
    let mb = |kind, text: &str, level| MdBlock { kind, text: text.into(), level, bold: false, italic: false, indent: 0 };
    st.set_note_blocks(model(vec![
        mb(0, "Derivatives", 1),
        mb(1, "The power rule says d/dx x^n = n x^(n-1).", 0),
        mb(2, "• sin → cos", 0),
        mb(2, "• cos → -sin", 0),
        mb(3, "f(x) = x^2", 0),
    ]));
    st.set_note_filter_names(model(vec!["All subjects".into(), "Mathematics".into(), "Biology".into()]));
    // stats
    st.set_stats_today("2h 05m".into());
    st.set_stats_week("9h 40m".into());
    st.set_stats_month("31h 10m".into());
    st.set_stats_all("88h 30m".into());
    st.set_stats_day_bars(model((1..=14).map(|i| bar(&format!("{i}"), (i % 5 + 1) as f32 / 5.0, &format!("{}m", (i % 5 + 1) * 30), if i % 3 == 0 { c(62, 194, 143) } else { c(108, 140, 255) })).collect()));
    st.set_stats_hour_bars(model((0..24).map(|h| bar(if h % 3 == 0 { "0" } else { "" }, if (9..18).contains(&h) { 0.6 } else { 0.1 }, "", c(239, 111, 108))).collect()));
    st.set_stats_app_bars(model(vec![bar("discord", 1.0, "12", c(239, 111, 108)), bar("reddit.com", 0.4, "5", c(239, 111, 108))]));
    st.set_stats_sessions(model(vec![SessionRow { when: "Nov 24 13:10".into(), subject: "Mathematics".into(), goal: "Finish integration exercises".into(), duration: "50m".into(), outcome: "Done".into() }]));
    st.set_stats_distractions(model(vec![DistractionRow { when: "Nov 24 13:44".into(), target: "discord".into(), list: "Social Media".into(), action: "killed".into() }]));
    demo_blocking(&st);
}

#[allow(unused_variables)]
fn demo_blocking(st: &AppState) {}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "/tmp".into());
    let light = std::env::args().nth(2).as_deref() == Some("light");
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(P(window.clone()))).unwrap();
    let (w, h) = (1200u32, 780u32);
    window.set_size(PhysicalSize::new(w, h));
    let ui = AppWindow::new().unwrap();
    demo(&ui);
    if light {
        ui.global::<Theme>().set_mode(1);
    }
    ui.show().unwrap();
    let names = ["dashboard", "focus", "flashcards", "notes", "tasks", "blocking", "stats", "settings"];
    let _ = Rgb565Pixel::default();
    for (i, n) in names.iter().enumerate() {
        ui.set_page(i as i32);
        slint::platform::update_timers_and_animations();
        let mut buf = vec![slint::Rgb8Pixel::default(); (w * h) as usize];
        window.draw_if_needed(|r| {
            r.render(&mut buf, w as usize);
        });
        let path = format!("{out}/{n}.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let bytes: Vec<u8> = buf.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        enc.write_header().unwrap().write_image_data(&bytes).unwrap();
        println!("wrote {path}");
    }
}
