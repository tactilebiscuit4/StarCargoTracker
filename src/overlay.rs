//! A transparent, click-through window drawn over the game that shows the
//! tracker's status and recent trades. It's a normal window kept on top, so
//! it only shows over the game in Borderless Windowed mode, and it never
//! touches the game itself.
//!
//! The window must run on the main thread and can only be created once per
//! run, so it's created the first time it's switched on and afterwards just
//! hidden and shown. Tracking and the `cargo>` prompt talk to it through the
//! functions below.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText};

use crate::format;
use crate::live::{self, Notice, SaleResult};
use crate::parser::Side;
use crate::tracker::Outcome;
use crate::uex;

/// Also used to find our own window when moving it.
const TITLE: &str = "Star Cargo Tracker Overlay";
/// The window grows and shrinks to fit its text, but never below this.
const MIN_SIZE: egui::Vec2 = egui::vec2(120.0, 40.0);
/// How long a trade stays on screen, the last part of it fading out.
const SHOW_FOR: Duration = Duration::from_secs(10);
const FADE_FOR: Duration = Duration::from_secs(2);
const MAX_ITEMS: usize = 5;
/// How often to check which monitor the game is on.
const REPOSITION_EVERY: Duration = Duration::from_secs(2);
/// Share Tech Mono is narrow, so a little larger than usual for readability.
const TEXT_SIZE: f32 = 17.0;

const WHITE: Color32 = Color32::from_rgb(235, 235, 235);
const GREY: Color32 = Color32::from_rgb(160, 160, 160);
const BLUE: Color32 = Color32::from_rgb(110, 180, 255);
const GREEN: Color32 = Color32::from_rgb(110, 220, 130);
const AMBER: Color32 = Color32::from_rgb(240, 180, 80);
const RED: Color32 = Color32::from_rgb(240, 95, 95);

/// Whether the user wants the overlay showing.
static WANT_VISIBLE: AtomicBool = AtomicBool::new(false);
/// Set once the main thread has been asked to create the window.
static START_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Wakes the main thread to create the window; see `start_requests`.
static START: OnceLock<Sender<()>> = OnceLock::new();

/// What the tracking thread and the window share.
struct Shared {
    /// For waking the window; set once it exists.
    ctx: Option<egui::Context>,
    /// Notices for the window; dropped when tracking finishes, which closes it.
    notices: Option<Sender<Notice>>,
    /// The latest status, so a window opened later starts with it.
    status: Status,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    ctx: None,
    notices: None,
    status: Status::Running,
});

/// The main thread waits on this, then calls `run_window` on the first
/// request.
pub fn start_requests() -> Receiver<()> {
    let (sender, receiver) = mpsc::channel();
    let _ = START.set(sender);
    receiver
}

/// Shows or hides the overlay, creating it the first time it's shown.
pub fn set_visible(visible: bool) {
    WANT_VISIBLE.store(visible, Ordering::SeqCst);
    if visible && !START_REQUESTED.swap(true, Ordering::SeqCst) {
        if let Some(start) = START.get() {
            let _ = start.send(());
        }
        return;
    }
    wake();
}

pub fn is_visible() -> bool {
    WANT_VISIBLE.load(Ordering::SeqCst)
}

/// Passes a tracking notice to the window, if it exists.
pub fn forward(notice: &Notice) {
    let mut shared = SHARED.lock().unwrap();
    match notice {
        Notice::Watching(_) | Notice::LogFound => shared.status = Status::Running,
        Notice::WaitingForLog(_) => shared.status = Status::WaitingForLog,
        Notice::Stopping => shared.status = Status::Stopping,
        _ => {}
    }
    if let Some(notices) = &shared.notices {
        let _ = notices.send(notice.clone());
    }
    if let Some(ctx) = &shared.ctx {
        ctx.request_repaint();
    }
}

/// Tracking has stopped: close the window, if it exists.
pub fn tracking_finished() {
    SHARED.lock().unwrap().notices = None;
    wake();
}

fn wake() {
    if let Some(ctx) = &SHARED.lock().unwrap().ctx {
        ctx.request_repaint();
    }
}

/// Creates and runs the window on the calling (main) thread until tracking
/// finishes.
pub fn run_window() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(MIN_SIZE)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(false)
            .with_icon(icon()),
        ..Default::default()
    };

    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_fonts(fonts());
            let (sender, receiver) = mpsc::channel();
            let mut shared = SHARED.lock().unwrap();
            shared.ctx = Some(cc.egui_ctx.clone());
            shared.notices = Some(sender);
            Ok(Box::new(Overlay::new(receiver, shared.status)))
        }),
    );

    // However the window closed, make sure tracking finishes too.
    live::request_stop();
    result.map_err(|e| e.to_string())
}

/// The program's icon (assets/icon), for where Windows shows the window,
/// such as Alt+Tab.
fn icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/icon-256.png")).unwrap_or_default()
}

/// Share Tech Mono for a HUD look (SIL Open Font License, see
/// assets/fonts/ShareTechMono-OFL.txt), with egui's built-in fonts kept as a
/// fallback for any character it lacks.
fn fonts() -> egui::FontDefinitions {
    const NAME: &str = "Share Tech Mono";
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        NAME.to_string(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/ShareTechMono-Regular.ttf"
        ))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, NAME.to_string());
    }
    fonts
}

#[derive(Clone, Copy)]
enum Status {
    Running,
    WaitingForLog,
    Stopping,
}

/// One notice on screen, as lines of coloured text.
struct Item {
    lines: Vec<(String, Color32)>,
    shown_at: Instant,
}

struct Overlay {
    notices: Receiver<Notice>,
    status: Status,
    /// Whether the window is currently shown (it starts shown).
    visible: bool,
    /// Oldest first.
    items: Vec<Item>,
    /// Window size needed to fit the text, in points.
    size: egui::Vec2,
    /// `None` forces a move/resize on the next frame.
    last_reposition: Option<Instant>,
}

impl Overlay {
    fn new(notices: Receiver<Notice>, status: Status) -> Self {
        Self {
            notices,
            status,
            visible: true,
            items: Vec::new(),
            size: MIN_SIZE,
            last_reposition: None,
        }
    }

    fn handle(&mut self, notice: Notice) {
        let lines = match notice {
            Notice::Watching(_) | Notice::LogFound => {
                self.status = Status::Running;
                return;
            }
            Notice::WaitingForLog(_) => {
                self.status = Status::WaitingForLog;
                return;
            }
            Notice::Stopping => {
                self.status = Status::Stopping;
                return;
            }
            Notice::NewSession => vec![("New game session".to_string(), GREY)],
            Notice::ShipNamed { class, name, .. } => vec![(
                format!("New {} named \"{name}\" (replaces the destroyed one)", crate::ships::class_name(&class)),
                GREY,
            )],
            Notice::Trade(trade) => trade_lines(&trade),
            Notice::UexSync(report) => sync_lines(&report),
            Notice::ShipDestroyed(loss) => {
                let mut lines = vec![(format!("{} destroyed", loss.ship_name), RED)];
                if loss.lost.is_empty() {
                    lines.push(("No tracked cargo aboard".to_string(), GREY));
                }
                for cargo in &loss.lost {
                    lines.push((
                        format!(
                            "Wrote off {} SCU {}  {}",
                            format::scu(cargo.scu),
                            cargo.commodity,
                            format::signed_auec(-cargo.cost)
                        ),
                        RED,
                    ));
                }
                lines
            }
        };
        self.items.push(Item {
            lines,
            shown_at: Instant::now(),
        });
        if self.items.len() > MAX_ITEMS {
            self.items.remove(0);
        }
    }
}

/// What a send to your UEX account did, e.g. "Synced to UEX: #21, #22".
fn sync_lines(report: &crate::uex_sync::Report) -> Vec<(String, Color32)> {
    let test = if report.test { " (test mode)" } else { "" };
    let mut lines = Vec::new();
    if !report.sent_trades.is_empty() {
        lines.push((format!("Synced to UEX{test}: {}", format::trade_ids(&report.sent_trades)), GREEN));
    }
    if !report.reversed_trades.is_empty() {
        lines.push((format!("Reversed on UEX{test}: {}", format::trade_ids(&report.reversed_trades)), GREY));
    }
    for rejected in &report.rejected {
        // Per-trade rejections start with the trade's ID; account problems don't.
        let text = if rejected.starts_with('#') { format!("UEX rejected {rejected}") } else { rejected.clone() };
        lines.push((text, AMBER));
    }
    if report.offline.as_deref().is_some_and(|why| why != "waiting to retry") {
        lines.push(("Can't reach UEX; will retry".to_string(), AMBER));
    }
    lines
}

fn trade_lines(trade: &live::TradeNotice) -> Vec<(String, Color32)> {
    let (t, failure) = match &trade.outcome {
        Outcome::Confirmed(t) => (t, None),
        Outcome::Failed(t, reason) => (t, Some(reason)),
    };
    let mut what = format!(
        "{} {} SCU {} @ {}",
        t.side,
        format::scu(t.scu),
        trade.commodity,
        trade.place
    );
    if let Some(ship) = &trade.ship {
        what.push_str(&format!(" ({ship})"));
    }
    if let Some(reason) = failure {
        return vec![(format!("FAILED {what} ({reason})"), AMBER)];
    }

    let (sign, colour) = match t.side {
        Side::Buy => (-1.0, BLUE),
        Side::Sell => (1.0, GREEN),
    };
    // The ID is what `cancel-trade` needs if the sale doesn't go through.
    let mut lines = vec![(
        format!("{what}  {}  #{}", format::signed_auec(sign * t.total_price), trade.id),
        colour,
    )];
    match &trade.sale {
        Some(SaleResult::Profit { profit, margin }) => lines.push((
            format!("Profit {}{}", format::signed_auec(*profit), format::margin(*margin)),
            if *profit >= 0.0 { GREEN } else { RED },
        )),
        Some(SaleResult::Unknown) => {
            lines.push(("Profit unknown (no recorded purchase)".to_string(), GREY))
        }
        None => {}
    }
    if let Some(note) = &trade.market {
        let (text, tone) = note.describe();
        let colour = match tone {
            uex::Tone::Good => GREEN,
            uex::Tone::Bad => AMBER,
            uex::Tone::Neutral => GREY,
        };
        lines.push((text, colour));
    }
    lines
}

impl eframe::App for Overlay {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // Fully transparent apart from what we draw.
    }

    /// Runs before every `ui`, and also while the window is hidden.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        loop {
            match self.notices.try_recv() {
                Ok(notice) => self.handle(notice),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Tracking has stopped.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    break;
                }
            }
        }

        let want_visible = is_visible();
        if want_visible != self.visible {
            self.visible = want_visible;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(want_visible));
            self.last_reposition = None; // The game may have moved meanwhile.
        }
        if !self.visible {
            // Don't pile up trades to show all at once later.
            self.items.clear();
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.items.retain(|item| item.shown_at.elapsed() < SHOW_FOR);

        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        let content = ui.with_layout(Layout::top_down(Align::Max), |ui| {
            let (text, dot) = match self.status {
                Status::Running => ("Tracking Cargo", GREEN),
                Status::WaitingForLog => ("Waiting for Game.log", AMBER),
                Status::Stopping => ("Stopping...", GREY),
            };
            panel(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 5.0, dot);
                    ui.label(RichText::new(text).color(WHITE).size(TEXT_SIZE));
                });
            });

            // Newest first, fading out at the end of their time.
            for item in self.items.iter().rev() {
                let left = SHOW_FOR.saturating_sub(item.shown_at.elapsed());
                let opacity = (left.as_secs_f32() / FADE_FOR.as_secs_f32()).min(1.0);
                ui.scope(|ui| {
                    ui.multiply_opacity(opacity);
                    panel(ui, |ui| {
                        for (line, colour) in &item.lines {
                            ui.label(RichText::new(line).color(*colour).size(TEXT_SIZE));
                        }
                    });
                });
            }
        });

        // Text wider or taller than the window spills past its edges and is
        // cut off, so fit the window to the text. Small changes are ignored
        // to avoid jitter.
        let needed = content.response.rect.size().max(MIN_SIZE).ceil();
        if (needed - self.size).abs().max_elem() > 1.0 {
            self.size = needed;
            self.last_reposition = None;
            ctx.request_repaint(); // Redraw at the new size straight away.
        }
        if self.last_reposition.is_none_or(|at| at.elapsed() >= REPOSITION_EVERY) {
            place(&ctx, self.size);
            self.last_reposition = Some(Instant::now());
        }

        // Animate fades smoothly; otherwise just check for news a few times a second.
        let fading = self
            .items
            .iter()
            .any(|item| SHOW_FOR.saturating_sub(item.shown_at.elapsed()) < FADE_FOR);
        ctx.request_repaint_after(Duration::from_millis(if fading { 33 } else { 250 }));
    }
}

/// A dark, rounded backing so text is readable over any part of the game.
fn panel(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::from_black_alpha(170))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, add_contents);
    ui.add_space(4.0);
}

/// Sizes the overlay to `size` (in points) and moves it to the top-right of
/// the game's monitor.
#[cfg(windows)]
fn place(ctx: &egui::Context, size: egui::Vec2) {
    let pixels = size * ctx.pixels_per_point();
    position::place_on_game_monitor(TITLE, pixels.x.ceil() as i32, pixels.y.ceil() as i32);
}

/// Elsewhere there's no portable way to find the game's window, so this uses
/// the monitor the overlay is on. Wayland lets apps resize themselves but not
/// choose where they go, so there the compositor decides the position.
#[cfg(not(windows))]
fn place(ctx: &egui::Context, size: egui::Vec2) {
    /// Gap between the overlay and the screen edge, in points.
    const MARGIN: f32 = 24.0;
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
    if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
        let at = egui::pos2(monitor.x - size.x - MARGIN, MARGIN);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(at));
    }
}

/// Finding the game's monitor and moving the overlay there, via Win32.
#[cfg(windows)]
mod position {
    use std::ptr;

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
        MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FindWindowW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId,
        IsWindowVisible, LoadImageW, SendMessageW, SetWindowPos, HWND_TOPMOST, ICON_BIG,
        ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
        SWP_NOACTIVATE, WM_SETICON,
    };

    const GAME_EXE: &str = "StarCitizen.exe";
    /// Gap between the overlay and the screen edge, in pixels.
    const MARGIN: i32 = 24;
    /// The icon's resource ID in the exe (see build.rs).
    const ICON_ID: u16 = 1;

    static ICONS_SET: AtomicBool = AtomicBool::new(false);

    /// eframe sets only the small icon, from one large image that Windows
    /// shrinks; Alt+Tab shows the large one. This sets both from the icon
    /// built into the exe, whose small sizes are drawn by hand.
    ///
    /// SAFETY: `window` must be a valid window handle.
    unsafe fn set_icons(window: HWND) {
        if ICONS_SET.swap(true, Ordering::SeqCst) {
            return;
        }
        for (kind, width, height) in [(ICON_BIG, SM_CXICON, SM_CYICON), (ICON_SMALL, SM_CXSMICON, SM_CYSMICON)] {
            let icon = LoadImageW(
                GetModuleHandleW(ptr::null()),
                ICON_ID as usize as *const u16, // MAKEINTRESOURCEW
                IMAGE_ICON,
                GetSystemMetrics(width),
                GetSystemMetrics(height),
                LR_DEFAULTCOLOR,
            );
            if !icon.is_null() {
                SendMessageW(window, WM_SETICON, kind as usize, icon as isize);
            }
        }
    }

    /// Sizes the overlay to `width` x `height` pixels and moves it to the
    /// top-right of the monitor the game is on, or of the primary monitor if
    /// the game isn't running. Done in one call so the text doesn't jump
    /// while the window catches up.
    pub fn place_on_game_monitor(title: &str, width: i32, height: i32) {
        let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
        // SAFETY: plain Win32 calls with valid, initialised arguments; the
        // window and monitor handles come straight from Windows.
        unsafe {
            let overlay = FindWindowW(ptr::null(), title.as_ptr());
            if overlay.is_null() {
                return;
            }
            set_icons(overlay);
            let monitor = match find_game_window() {
                Some(game) => MonitorFromWindow(game, MONITOR_DEFAULTTONEAREST),
                None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
            };
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            let mut ours: RECT = std::mem::zeroed();
            if GetMonitorInfoW(monitor, &mut info) == 0 || GetWindowRect(overlay, &mut ours) == 0 {
                return;
            }

            let area = info.rcWork; // Excludes the taskbar.
            let x = area.right - width - MARGIN;
            let y = area.top + MARGIN;
            let current = (ours.left, ours.top, ours.right - ours.left, ours.bottom - ours.top);
            if current != (x, y, width, height) {
                SetWindowPos(overlay, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
            }
        }
    }

    fn find_game_window() -> Option<HWND> {
        let mut found: HWND = ptr::null_mut();
        // SAFETY: `check_window` only writes through the pointer we pass,
        // which outlives the call.
        unsafe { EnumWindows(Some(check_window), &mut found as *mut HWND as LPARAM) };
        (!found.is_null()).then_some(found)
    }

    unsafe extern "system" fn check_window(window: HWND, found: LPARAM) -> BOOL {
        if IsWindowVisible(window) == 0 {
            return 1; // Keep looking.
        }
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if process_file_name(pid).is_some_and(|name| name.eq_ignore_ascii_case(GAME_EXE)) {
            *(found as *mut HWND) = window;
            return 0; // Stop.
        }
        1
    }

    /// e.g. `StarCitizen.exe`
    fn process_file_name(pid: u32) -> Option<String> {
        // SAFETY: the buffer and length are valid for the call, and the
        // process handle is closed before returning.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let mut buffer = [0u16; 1024];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len);
            CloseHandle(process);
            if ok == 0 {
                return None;
            }
            let path = String::from_utf16_lossy(&buffer[..len as usize]);
            path.rsplit(['\\', '/']).next().map(str::to_string)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uex_sync::Report;

    #[test]
    fn loads_the_window_icon() {
        let icon = icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    #[test]
    fn shows_what_a_sync_did() {
        let report = Report {
            sent: 4,
            sent_trades: vec![21, 22],
            reversed: 1,
            reversed_trades: vec![15],
            rejected: vec!["#23 wallet: invalid_price".into()],
            ..Report::default()
        };
        assert_eq!(
            sync_lines(&report),
            vec![
                ("Synced to UEX: #21, #22".to_string(), GREEN),
                ("Reversed on UEX: #15".to_string(), GREY),
                ("UEX rejected #23 wallet: invalid_price".to_string(), AMBER),
            ]
        );

        let test_mode = Report { sent: 1, sent_trades: vec![30], test: true, ..Report::default() };
        assert_eq!(sync_lines(&test_mode), vec![("Synced to UEX (test mode): #30".to_string(), GREEN)]);

        let offline = Report { offline: Some("timeout".into()), ..Report::default() };
        assert_eq!(sync_lines(&offline), vec![("Can't reach UEX; will retry".to_string(), AMBER)]);
    }
}
