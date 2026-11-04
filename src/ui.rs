//! The window. The thread that calls `run` owns the window and timestamps
//! input; a second thread keeps the estimators and draws with GDI.

use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::device::{Connection, DeviceInfo};
use crate::estimate::{
    interval_bin_edge_us, Confidence, KeyboardEstimator, KeyboardReport, KeyboardResult,
    MouseEstimator, MouseQuality, MouseReport, GAP_BINS, GAP_BIN_US, INTERVAL_BINS, MIN_GAPS,
    STANDARD_RATES,
};
use crate::input::{self, Capture, InputEvent, InputKind};

const FRAME: Duration = Duration::from_millis(33);
const IDLE_REDRAW: Duration = Duration::from_millis(1000);
const CHART_SPAN_NS: u64 = 10_000_000_000;
const CHART_BREAK_NS: u64 = 150_000_000;
const MIN_WIDTH: i32 = 860;
const MIN_HEIGHT: i32 = 640;

struct WindowState {
    capture: Arc<Capture>,
    clicks: Mutex<Vec<(i32, i32)>>,
    width: AtomicI32,
    height: AtomicI32,
    dpi: AtomicU32,
    repaint: AtomicBool,
    running: AtomicBool,
}

static STATE: OnceLock<Arc<WindowState>> = OnceLock::new();

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[derive(Clone, Copy)]
struct Theme {
    bg: COLORREF,
    panel: COLORREF,
    selected: COLORREF,
    border: COLORREF,
    grid: COLORREF,
    text: COLORREF,
    muted: COLORREF,
    accent: COLORREF,
    good: COLORREF,
    warn: COLORREF,
    warn_bg: COLORREF,
}

const DARK: Theme = Theme {
    bg: rgb(0x15, 0x17, 0x1B),
    panel: rgb(0x1E, 0x21, 0x26),
    selected: rgb(0x2A, 0x2F, 0x37),
    border: rgb(0x34, 0x38, 0x40),
    grid: rgb(0x2C, 0x30, 0x37),
    text: rgb(0xEC, 0xEE, 0xF1),
    muted: rgb(0xA0, 0xA6, 0xB0),
    accent: rgb(0x62, 0xB0, 0xFF),
    good: rgb(0x5B, 0xC9, 0x8C),
    warn: rgb(0xF0, 0xB4, 0x4C),
    warn_bg: rgb(0x3A, 0x2F, 0x1A),
};

const LIGHT: Theme = Theme {
    bg: rgb(0xF5, 0xF6, 0xF8),
    panel: rgb(0xFF, 0xFF, 0xFF),
    selected: rgb(0xE4, 0xEC, 0xF7),
    border: rgb(0xD5, 0xD9, 0xE0),
    grid: rgb(0xE6, 0xE9, 0xEE),
    text: rgb(0x1B, 0x1E, 0x24),
    muted: rgb(0x5C, 0x63, 0x6E),
    accent: rgb(0x1A, 0x6F, 0xD0),
    good: rgb(0x13, 0x80, 0x4A),
    warn: rgb(0x8F, 0x5B, 0x00),
    warn_bg: rgb(0xFC, 0xF0, 0xD6),
};

fn system_uses_dark() -> bool {
    let key = w(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = w("AppsUseLightTheme");
    let mut data = 1u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    status == 0 && data == 0
}

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mouse,
    Keyboard,
}

#[derive(Clone, Copy)]
enum Action {
    SelectTab(Tab),
    Reset,
    SelectDevice(usize),
}

struct MouseDevice {
    handle: usize,
    info: DeviceInfo,
    est: MouseEstimator,
    last_seen: u64,
}

struct KeyDevice {
    handle: usize,
    info: DeviceInfo,
    est: KeyboardEstimator,
    last_seen: u64,
}

#[derive(Default)]
struct Model {
    infos: HashMap<usize, DeviceInfo>,
    mice: Vec<MouseDevice>,
    keyboards: Vec<KeyDevice>,
    pinned_mouse: Option<usize>,
    pinned_keyboard: Option<usize>,
    latest: u64,
}

impl Model {
    fn info(&mut self, handle: usize) -> DeviceInfo {
        self.infos
            .entry(handle)
            .or_insert_with(|| DeviceInfo::from_handle(handle))
            .clone()
    }

    fn feed(&mut self, capture: &Capture, events: &[InputEvent]) {
        for event in events {
            let t = capture.ticks_to_ns(event.ticks);
            self.latest = self.latest.max(t);
            match event.kind {
                InputKind::Mouse => {
                    let at = match self.mice.iter().position(|d| d.handle == event.device) {
                        Some(at) => at,
                        None => {
                            let info = self.info(event.device);
                            self.mice.push(MouseDevice { handle: event.device, info, est: MouseEstimator::new(), last_seen: t });
                            self.mice.len() - 1
                        }
                    };
                    let device = &mut self.mice[at];
                    device.est.push(t);
                    device.last_seen = t;
                }
                InputKind::Key { code, down } => {
                    let at = match self.keyboards.iter().position(|d| d.handle == event.device) {
                        Some(at) => at,
                        None => {
                            let info = self.info(event.device);
                            self.keyboards.push(KeyDevice { handle: event.device, info, est: KeyboardEstimator::new(), last_seen: t });
                            self.keyboards.len() - 1
                        }
                    };
                    let device = &mut self.keyboards[at];
                    device.est.push(t, code, down);
                    device.last_seen = t;
                }
            }
        }
    }

    fn selected_mouse(&self) -> Option<&MouseDevice> {
        self.pinned_mouse
            .and_then(|h| self.mice.iter().find(|d| d.handle == h))
            .or_else(|| self.mice.iter().max_by_key(|d| d.last_seen))
    }

    fn selected_keyboard(&self) -> Option<&KeyDevice> {
        self.pinned_keyboard
            .and_then(|h| self.keyboards.iter().find(|d| d.handle == h))
            .or_else(|| self.keyboards.iter().max_by_key(|d| d.last_seen))
    }

    fn reset(&mut self) {
        self.mice.clear();
        self.keyboards.clear();
        self.pinned_mouse = None;
        self.pinned_keyboard = None;
    }
}

// -------------------------------------------------------------- painter

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Font {
    Small,
    Body,
    BodyBold,
    Title,
    Stat,
    Huge,
}

struct Painter {
    dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    width: i32,
    height: i32,
    dpi: u32,
    fonts: HashMap<Font, HFONT>,
    theme: Theme,
}

impl Painter {
    fn new(window_dc: HDC, width: i32, height: i32, dpi: u32, theme: Theme) -> Self {
        unsafe {
            let dc = CreateCompatibleDC(window_dc);
            let bitmap = CreateCompatibleBitmap(window_dc, width.max(1), height.max(1));
            let old_bitmap = SelectObject(dc, bitmap);
            SetBkMode(dc, TRANSPARENT as _);
            let mut painter = Self { dc, bitmap, old_bitmap, width, height, dpi, fonts: HashMap::new(), theme };
            for (font, px, weight) in [
                (Font::Small, 12, 400),
                (Font::Body, 14, 400),
                (Font::BodyBold, 14, 600),
                (Font::Title, 18, 600),
                (Font::Stat, 20, 600),
                (Font::Huge, 58, 600),
            ] {
                let face = w("Segoe UI");
                let handle = CreateFontW(
                    -painter.s(px), 0, 0, 0, weight, 0, 0, 0,
                    DEFAULT_CHARSET as _, OUT_DEFAULT_PRECIS as _, CLIP_DEFAULT_PRECIS as _,
                    CLEARTYPE_QUALITY as _, (DEFAULT_PITCH as u32 | FF_DONTCARE as u32) as _, face.as_ptr(),
                );
                painter.fonts.insert(font, handle);
            }
            painter
        }
    }

    /// Scales a logical pixel length to the window's DPI.
    fn s(&self, px: i32) -> i32 {
        px * self.dpi as i32 / 96
    }

    fn fill(&self, r: Rect, color: COLORREF) {
        unsafe {
            SetDCBrushColor(self.dc, color);
            let rect = RECT { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h };
            FillRect(self.dc, &rect, GetStockObject(DC_BRUSH) as HBRUSH);
        }
    }

    fn panel(&self, r: Rect) {
        let t = self.s(1).max(1);
        self.fill(r, self.theme.border);
        self.fill(Rect { x: r.x + t, y: r.y + t, w: r.w - 2 * t, h: r.h - 2 * t }, self.theme.panel);
    }

    fn select_font(&self, font: Font) {
        unsafe { SelectObject(self.dc, self.fonts[&font] as HGDIOBJ) };
    }

    fn text(&self, x: i32, y: i32, s: &str, font: Font, color: COLORREF) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        self.select_font(font);
        unsafe {
            SetTextColor(self.dc, color);
            TextOutW(self.dc, x, y, wide.as_ptr(), wide.len() as i32);
        }
    }

    fn measure(&self, s: &str, font: Font) -> (i32, i32) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        self.select_font(font);
        let mut size = SIZE { cx: 0, cy: 0 };
        unsafe { GetTextExtentPoint32W(self.dc, wide.as_ptr(), wide.len() as i32, &mut size) };
        (size.cx, size.cy)
    }

    /// Draws text cut to a width with an ellipsis.
    fn text_clipped(&self, r: Rect, s: &str, font: Font, color: COLORREF) {
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        let mut rect = RECT { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h };
        self.select_font(font);
        unsafe {
            SetTextColor(self.dc, color);
            DrawTextW(
                self.dc,
                wide.as_mut_ptr(),
                wide.len() as i32,
                &mut rect,
                DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
    }

    fn polyline(&self, points: &[POINT], color: COLORREF, width: i32) {
        if points.len() < 2 {
            return;
        }
        unsafe {
            let pen = CreatePen(PS_SOLID, width, color);
            let old = SelectObject(self.dc, pen as HGDIOBJ);
            Polyline(self.dc, points.as_ptr(), points.len() as i32);
            SelectObject(self.dc, old);
            DeleteObject(pen as HGDIOBJ);
        }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        unsafe {
            for font in self.fonts.values() {
                DeleteObject(*font as HGDIOBJ);
            }
            SelectObject(self.dc, self.old_bitmap);
            DeleteObject(self.bitmap as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

// --------------------------------------------------------------- render

fn fmt_rate(hz: f64) -> String {
    format!("{hz:.0} Hz")
}

fn fmt_interval(us: f64) -> String {
    if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{us:.0} \u{b5}s")
    }
}

fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn device_subtitle(info: &DeviceInfo) -> String {
    let link = match info.connection {
        Connection::Usb => "USB",
        Connection::Bluetooth => "Bluetooth",
        Connection::Other => "Other",
    };
    match info.vid_pid {
        Some((vid, pid)) => format!("{vid:04X}:{pid:04X} \u{b7} {link}"),
        None => link.to_string(),
    }
}

fn descriptor_line(info: &DeviceInfo) -> String {
    match (info.connection, info.endpoint) {
        (_, Some(ep)) => format!(
            "USB descriptor requests {} ({}, {}). For a wireless dongle this describes the dongle.",
            fmt_rate(ep.rate_hz()),
            fmt_interval(ep.interval_us),
            ep.speed.label()
        ),
        (Connection::Bluetooth, _) => "Bluetooth device: no USB descriptor to compare against.".into(),
        (Connection::Usb, None) => "USB device, but its descriptor could not be read.".into(),
        (Connection::Other, None) => "No USB descriptor is available for this device.".into(),
    }
}

/// Rounds up to a tidy axis maximum.
fn nice_max(v: f64) -> f64 {
    let mut step = 100.0;
    while step < v {
        step *= if (step as u32).to_string().starts_with('1') { 2.0 } else if (step as u32).to_string().starts_with('2') { 2.5 } else { 2.0 };
    }
    step
}

struct Frame<'a> {
    p: &'a Painter,
    hits: Vec<(Rect, Action)>,
}

impl Frame<'_> {
    fn button(&mut self, r: Rect, label: &str, action: Action) {
        let p = self.p;
        p.panel(r);
        let (tw, th) = p.measure(label, Font::Body);
        p.text(r.x + (r.w - tw) / 2, r.y + (r.h - th) / 2, label, Font::Body, p.theme.text);
        self.hits.push((r, action));
    }

    fn tab(&mut self, x: i32, y: i32, label: &str, active: bool, action: Action) -> i32 {
        let p = self.p;
        let (tw, th) = p.measure(label, Font::Title);
        let color = if active { p.theme.text } else { p.theme.muted };
        p.text(x, y, label, Font::Title, color);
        if active {
            p.fill(Rect { x, y: y + th + p.s(3), w: tw, h: p.s(2).max(2) }, p.theme.accent);
        }
        self.hits.push((Rect { x, y: y - p.s(4), w: tw, h: th + p.s(10) }, action));
        tw
    }

    fn stat(&self, x: i32, y: i32, label: &str, value: &str) {
        let p = self.p;
        p.text(x, y, label, Font::Small, p.theme.muted);
        p.text(x, y + p.s(16), value, Font::Stat, p.theme.text);
    }

    fn banner(&self, r: Rect, s: &str, warn: bool) {
        let p = self.p;
        if warn {
            p.fill(r, p.theme.warn_bg);
            p.text_clipped(Rect { x: r.x + p.s(12), y: r.y + p.s(7), w: r.w - p.s(24), h: r.h }, s, Font::Body, p.theme.warn);
        } else {
            p.text_clipped(Rect { x: r.x, y: r.y + p.s(7), w: r.w, h: r.h }, s, Font::Body, p.theme.muted);
        }
    }

    fn device_list(&mut self, r: Rect, rows: &[(usize, String, String)], selected: Option<usize>, empty: &str) {
        let p = self.p;
        p.panel(r);
        p.text(r.x + p.s(14), r.y + p.s(12), "Devices", Font::BodyBold, p.theme.text);
        let mut y = r.y + p.s(38);
        if rows.is_empty() {
            p.text_clipped(Rect { x: r.x + p.s(14), y, w: r.w - p.s(28), h: p.s(20) }, empty, Font::Small, p.theme.muted);
        }
        for (handle, name, sub) in rows {
            let row = Rect { x: r.x + p.s(6), y, w: r.w - p.s(12), h: p.s(50) };
            if selected == Some(*handle) {
                p.fill(row, p.theme.selected);
                p.fill(Rect { x: row.x, y: row.y, w: p.s(3), h: row.h }, p.theme.accent);
            }
            p.text_clipped(Rect { x: row.x + p.s(10), y: row.y + p.s(7), w: row.w - p.s(16), h: p.s(20) }, name, Font::Body, p.theme.text);
            p.text_clipped(Rect { x: row.x + p.s(10), y: row.y + p.s(27), w: row.w - p.s(16), h: p.s(18) }, sub, Font::Small, p.theme.muted);
            self.hits.push((row, Action::SelectDevice(*handle)));
            y += p.s(54);
            if y + p.s(50) > r.y + r.h {
                break;
            }
        }
    }

    /// Bars growing up from a baseline, used by both histograms.
    fn histogram(&self, r: Rect, bins: &[u32], title: &str, ticks: &[(f64, String)]) {
        let p = self.p;
        p.panel(r);
        p.text(r.x + p.s(14), r.y + p.s(10), title, Font::BodyBold, p.theme.text);
        let plot = Rect {
            x: r.x + p.s(14),
            y: r.y + p.s(38),
            w: r.w - p.s(28),
            h: r.h - p.s(38) - p.s(30),
        };
        p.fill(Rect { x: plot.x, y: plot.y + plot.h, w: plot.w, h: p.s(1).max(1) }, p.theme.border);
        for (at, label) in ticks {
            let x = plot.x + (at * plot.w as f64) as i32;
            p.fill(Rect { x, y: plot.y + plot.h, w: p.s(1).max(1), h: p.s(4) }, p.theme.border);
            let (tw, _) = p.measure(label, Font::Small);
            let lx = (x - tw / 2).clamp(plot.x, plot.x + plot.w - tw);
            p.text(lx, plot.y + plot.h + p.s(6), label, Font::Small, p.theme.muted);
        }
        let max = bins.iter().copied().max().unwrap_or(0);
        if max == 0 {
            return;
        }
        let n = bins.len() as i32;
        for (i, &count) in bins.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let i = i as i32;
            let x0 = plot.x + i * plot.w / n;
            let x1 = plot.x + (i + 1) * plot.w / n;
            // Square root scale, so rare intervals stay visible beside the main peak.
            let h = (((count as f64 / max as f64).sqrt() * plot.h as f64) as i32).max(p.s(2));
            p.fill(Rect { x: x0, y: plot.y + plot.h - h, w: (x1 - x0 - 1).max(1), h }, p.theme.accent);
        }
    }

    fn rate_chart(&self, r: Rect, samples: &[(u64, f64)], latest: u64) {
        let p = self.p;
        p.panel(r);
        p.text(r.x + p.s(14), r.y + p.s(10), "Rate over the last 10 seconds of movement", Font::BodyBold, p.theme.text);
        let plot = Rect {
            x: r.x + p.s(60),
            y: r.y + p.s(40),
            w: r.w - p.s(76),
            h: r.h - p.s(40) - p.s(16),
        };
        let peak = samples.iter().map(|s| s.1).fold(0.0, f64::max);
        let top = nice_max(peak * 1.05);
        for i in 0..=2 {
            let y = plot.y + plot.h - plot.h * i / 2;
            p.fill(Rect { x: plot.x, y, w: plot.w, h: p.s(1).max(1) }, p.theme.grid);
            let label = format!("{:.0}", top * i as f64 / 2.0);
            let (tw, th) = p.measure(&label, Font::Small);
            p.text(plot.x - tw - p.s(8), y - th / 2, &label, Font::Small, p.theme.muted);
        }
        let end = samples.last().map_or(latest, |s| s.0);
        let mut run: Vec<POINT> = Vec::new();
        let mut last_t = 0;
        let stroke = p.s(2).max(2);
        for &(t, hz) in samples {
            if t.saturating_add(CHART_SPAN_NS) < end {
                continue;
            }
            if t.saturating_sub(last_t) > CHART_BREAK_NS {
                p.polyline(&run, p.theme.accent, stroke);
                run.clear();
            }
            last_t = t;
            let age = (end - t) as f64 / CHART_SPAN_NS as f64;
            run.push(POINT {
                x: plot.x + plot.w - (age * plot.w as f64) as i32,
                y: plot.y + plot.h - (hz / top * plot.h as f64) as i32,
            });
        }
        p.polyline(&run, p.theme.accent, stroke);
    }

    fn score_bars(&self, r: Rect, report: &KeyboardReport) {
        let p = self.p;
        p.panel(r);
        p.text(r.x + p.s(14), r.y + p.s(10), "Grid alignment by candidate rate", Font::BodyBold, p.theme.text);
        let top = r.y + p.s(40);
        let row_h = ((r.h - p.s(52)) / STANDARD_RATES.len() as i32).max(p.s(14));
        let bar_x = r.x + p.s(78);
        let bar_w = r.w - p.s(78) - p.s(54);
        let chosen = match report.result {
            KeyboardResult::Resolved { rate, .. } => Some(rate),
            _ => None,
        };
        for (i, (&rate, &score)) in STANDARD_RATES.iter().zip(&report.scores).enumerate() {
            let y = top + i as i32 * row_h;
            let bar_h = (row_h - p.s(5)).min(p.s(14));
            let label = format!("{rate} Hz");
            let (tw, th) = p.measure(&label, Font::Small);
            let bar_y = y + (row_h - bar_h) / 2;
            let text_y = y + (row_h - th) / 2;
            p.text(bar_x - tw - p.s(8), text_y, &label, Font::Small, p.theme.muted);
            p.fill(Rect { x: bar_x, y: bar_y, w: bar_w, h: bar_h }, p.theme.grid);
            let color = if chosen == Some(rate) { p.theme.good } else { p.theme.accent };
            let filled = (score.clamp(0.0, 1.0) * bar_w as f64) as i32;
            p.fill(Rect { x: bar_x, y: bar_y, w: filled, h: bar_h }, color);
            if report.gaps > 0 {
                let mark = bar_x + (report.threshold.clamp(0.0, 1.0) * bar_w as f64) as i32;
                p.fill(Rect { x: mark, y: bar_y - p.s(1), w: p.s(2).max(2), h: bar_h + p.s(2) }, p.theme.text);
            }
            p.text(bar_x + bar_w + p.s(8), text_y, &format!("{score:.2}"), Font::Small, p.theme.muted);
        }
    }

    fn mouse_tab(&mut self, r: Rect, model: &Model, foreground: bool) {
        let p = self.p;
        let selected = model.selected_mouse();
        let report = selected.map(|d| d.est.report(model.latest));

        let (headline, status, status_color) = match &report {
            None => ("--".to_string(), "Waiting for mouse movement".to_string(), p.theme.muted),
            Some(rep) => match (rep.quality, rep.snapped, rep.measured_hz) {
                (MouseQuality::Collecting, ..) => ("--".to_string(), "Keep moving...".to_string(), p.theme.muted),
                (MouseQuality::Steady, Some(rate), _) => (format!("{rate} Hz"), "Steady reading".to_string(), p.theme.good),
                (MouseQuality::Steady, None, Some(hz)) => (fmt_rate(hz), "Steady, but not a standard rate".to_string(), p.theme.warn),
                (_, _, Some(hz)) => (fmt_rate(hz), "Uneven: move faster for a reliable reading".to_string(), p.theme.warn),
                _ => ("--".to_string(), String::new(), p.theme.muted),
            },
        };

        let prompt = "Move the mouse quickly in large circles for about 5 seconds. Press Reset after changing its rate.";
        let mut y = self.top_of_tab(r, prompt, foreground);
        p.text(r.x, y, &headline, Font::Huge, p.theme.text);
        let (hw, hh) = p.measure(&headline, Font::Huge);
        p.text(r.x + hw + p.s(20), y + hh / 2 - p.s(18), &status, Font::Body, status_color);
        if let Some(rep) = &report {
            let detail = match rep.measured_hz {
                Some(hz) => format!("Measured {hz:.1} Hz \u{b7} Live {:.0} Hz", rep.live_hz),
                None => format!("Live {:.0} Hz", rep.live_hz),
            };
            p.text(r.x + hw + p.s(20), y + hh / 2 + p.s(3), &detail, Font::Body, p.theme.muted);
        }
        y += hh + p.s(6);

        let MouseStatsText { median, p99, steady, reports } = mouse_stats(report.as_ref());
        let cell = r.w / 4;
        self.stat(r.x, y, "Median interval", &median);
        self.stat(r.x + cell, y, "99th percentile interval", &p99);
        self.stat(r.x + 2 * cell, y, "Windows at peak rate", &steady);
        self.stat(r.x + 3 * cell, y, "Reports", &reports);
        y += p.s(52);

        let line = selected.map_or_else(|| "The USB descriptor appears here once a device is active.".to_string(), |d| descriptor_line(&d.info));
        p.text_clipped(Rect { x: r.x, y, w: r.w, h: p.s(20) }, &line, Font::Small, p.theme.muted);
        y += p.s(24);

        let rest = r.y + r.h - y;
        let gap = p.s(12);
        let chart_h = (rest - gap) / 2;
        let samples: Vec<(u64, f64)> = selected.map_or_else(Vec::new, |d| d.est.samples().collect());
        self.rate_chart(Rect { x: r.x, y, w: r.w, h: chart_h }, &samples, model.latest);

        let bins = report.as_ref().map_or([0; INTERVAL_BINS], |rep| rep.histogram);
        let ticks: Vec<(f64, String)> = [(0usize), 10, 20, 30, 39]
            .iter()
            .map(|&bin| (bin as f64 / INTERVAL_BINS as f64, fmt_interval(interval_bin_edge_us(bin))))
            .collect();
        self.histogram(
            Rect { x: r.x, y: y + chart_h + gap, w: r.w, h: rest - chart_h - gap },
            &bins,
            "Time between reports (log scale)",
            &ticks,
        );
    }

    fn keyboard_tab(&mut self, r: Rect, model: &Model, foreground: bool) {
        let p = self.p;
        let selected = model.selected_keyboard();
        let report = selected.map(|d| d.est.report());

        let (headline, status, status_color) = match report.as_ref().map(|rep| rep.result) {
            None => ("--".to_string(), "Waiting for key presses".to_string(), p.theme.muted),
            Some(KeyboardResult::Collecting) => {
                let have = report.as_ref().map_or(0, |rep| rep.gaps);
                ("--".to_string(), format!("Keep going: {have} of {MIN_GAPS} usable gaps"), p.theme.muted)
            }
            Some(KeyboardResult::NotResolved) => (
                "Not resolved".to_string(),
                "Timing fits no standard grid. It may be too fine to see, or wireless.".to_string(),
                p.theme.warn,
            ),
            Some(KeyboardResult::Resolved { rate, confidence, score }) => {
                // More presses help only early on. After that the score reflects
                // timing noise, which more samples cannot reduce.
                let early = report.as_ref().is_some_and(|rep| rep.gaps < 150);
                let (label, color) = match (confidence, early) {
                    (Confidence::High, _) => ("High confidence", p.theme.good),
                    (Confidence::Medium, true) => ("Medium confidence: keep going", p.theme.good),
                    (Confidence::Medium, false) => ("Medium confidence: close to the timing limit", p.theme.good),
                    (Confidence::Low, true) => ("Low confidence: keep going", p.theme.warn),
                    (Confidence::Low, false) => ("Low confidence: noisy timing", p.theme.warn),
                };
                (format!("{rate} Hz"), format!("{label} (score {score:.2})"), color)
            }
        };

        let prompt = "Roll your fingers across letter keys, pressing and releasing fast. Avoid Windows and Alt.";
        let mut y = self.top_of_tab(r, prompt, foreground);
        p.text(r.x, y, &headline, Font::Huge, p.theme.text);
        let (hw, hh) = p.measure(&headline, Font::Huge);
        p.text_clipped(
            Rect { x: r.x + hw + p.s(20), y: y + hh / 2 - p.s(10), w: r.w - hw - p.s(20), h: p.s(22) },
            &status,
            Font::Body,
            status_color,
        );
        y += hh + p.s(6);

        // A reading below the descriptor can be a real finding, so explain it.
        let below_descriptor = match (report.as_ref().map(|rep| rep.result), selected.and_then(|d| d.info.endpoint)) {
            (Some(KeyboardResult::Resolved { rate, .. }), Some(ep)) => (rate as f64) < ep.rate_hz() * 0.85,
            _ => false,
        };

        let events = report.as_ref().map_or("0".to_string(), |rep| fmt_count(rep.events));
        let gaps = report.as_ref().map_or("0".to_string(), |rep| fmt_count(rep.gaps as u64));
        let min_gap = report.as_ref().and_then(|rep| rep.min_gap_us).map_or("--".to_string(), fmt_interval);
        let ignored = report.as_ref().map_or("0".to_string(), |rep| fmt_count(rep.repeats_ignored));
        let cell = r.w / 4;
        self.stat(r.x, y, "Key changes", &events);
        self.stat(r.x + cell, y, "Usable gaps", &gaps);
        self.stat(r.x + 2 * cell, y, "Shortest gap", &min_gap);
        self.stat(r.x + 3 * cell, y, "Auto-repeats ignored", &ignored);
        y += p.s(52);

        let line = match selected.and_then(|d| d.info.endpoint) {
            Some(ep) if below_descriptor => format!(
                "USB descriptor requests {}, but key changes arrive less often, possibly from slower key scanning.",
                fmt_rate(ep.rate_hz())
            ),
            _ => selected.map_or_else(|| "The USB descriptor appears here once a device is active.".to_string(), |d| descriptor_line(&d.info)),
        };
        p.text_clipped(Rect { x: r.x, y, w: r.w, h: p.s(20) }, &line, Font::Small, p.theme.muted);
        y += p.s(24);

        let rest = r.y + r.h - y;
        let gap = p.s(12);
        let chart_h = (rest - gap) * 55 / 100;
        let empty = KeyboardEstimator::new().report();
        self.score_bars(Rect { x: r.x, y, w: r.w, h: chart_h }, report.as_ref().unwrap_or(&empty));

        let bins = report.as_ref().map_or([0; GAP_BINS], |rep| rep.histogram);
        let span_ms = GAP_BINS as f64 * GAP_BIN_US / 1000.0;
        let ticks: Vec<(f64, String)> = (0..=5)
            .map(|i| (i as f64 / 5.0, format!("{:.0} ms", span_ms * i as f64 / 5.0)))
            .collect();
        self.histogram(
            Rect { x: r.x, y: y + chart_h + gap, w: r.w, h: rest - chart_h - gap },
            &bins,
            "Time between key changes (0 to 10 ms)",
            &ticks,
        );
    }

    /// Draws the prompt or paused banner and returns the y below it.
    fn top_of_tab(&self, r: Rect, prompt: &str, foreground: bool) -> i32 {
        let p = self.p;
        let band = Rect { x: r.x, y: r.y, w: r.w, h: p.s(32) };
        if foreground {
            self.banner(band, prompt, false);
        } else {
            self.banner(band, "Paused: Windows only sends input while this window is focused. Click here to resume.", true);
        }
        r.y + p.s(32)
    }
}

struct MouseStatsText {
    median: String,
    p99: String,
    steady: String,
    reports: String,
}

fn mouse_stats(report: Option<&MouseReport>) -> MouseStatsText {
    MouseStatsText {
        median: report.and_then(|r| r.median_interval_us).map_or("--".into(), fmt_interval),
        p99: report.and_then(|r| r.p99_interval_us).map_or("--".into(), fmt_interval),
        steady: report
            .filter(|r| r.measured_hz.is_some())
            .map_or("--".into(), |r| format!("{:.0}%", r.steady_fraction * 100.0)),
        reports: report.map_or("0".into(), |r| fmt_count(r.reports)),
    }
}

fn draw(p: &Painter, tab: Tab, model: &Model, foreground: bool) -> Vec<(Rect, Action)> {
    let mut frame = Frame { p, hits: Vec::new() };
    p.fill(Rect { x: 0, y: 0, w: p.width, h: p.height }, p.theme.bg);

    let margin = p.s(20);
    let header_y = p.s(16);
    let mut x = margin;
    x += frame.tab(x, header_y, "Mouse", tab == Tab::Mouse, Action::SelectTab(Tab::Mouse));
    x += p.s(24);
    frame.tab(x, header_y, "Keyboard", tab == Tab::Keyboard, Action::SelectTab(Tab::Keyboard));
    let reset_w = p.s(76);
    frame.button(Rect { x: p.width - margin - reset_w, y: p.s(12), w: reset_w, h: p.s(32) }, "Reset", Action::Reset);

    let body_y = p.s(58);
    let body_h = p.height - body_y - margin;
    let side_w = p.s(240);
    let side = Rect { x: margin, y: body_y, w: side_w, h: body_h };
    let main = Rect {
        x: margin + side_w + p.s(20),
        y: body_y,
        w: p.width - 2 * margin - side_w - p.s(20),
        h: body_h,
    };

    match tab {
        Tab::Mouse => {
            let rows: Vec<_> = model.mice.iter().map(|d| (d.handle, d.info.name.clone(), device_subtitle(&d.info))).collect();
            let chosen = model.selected_mouse().map(|d| d.handle);
            frame.device_list(side, &rows, chosen, "Move a mouse to list it.");
            frame.mouse_tab(main, model, foreground);
        }
        Tab::Keyboard => {
            let rows: Vec<_> = model.keyboards.iter().map(|d| (d.handle, d.info.name.clone(), device_subtitle(&d.info))).collect();
            let chosen = model.selected_keyboard().map(|d| d.handle);
            frame.device_list(side, &rows, chosen, "Press a key to list it.");
            frame.keyboard_tab(main, model, foreground);
        }
    }
    frame.hits
}

fn render_loop(hwnd: usize, state: Arc<WindowState>) {
    let hwnd = hwnd as HWND;
    let theme = if system_uses_dark() { DARK } else { LIGHT };
    let mut model = Model::default();
    let mut tab = Tab::Mouse;
    let mut hits: Vec<(Rect, Action)> = Vec::new();
    let mut events = Vec::with_capacity(4096);
    let mut painter: Option<Painter> = None;
    let mut next = Instant::now();
    let mut active_until = next;
    let mut drawn_foreground = true;

    while state.running.load(Ordering::Relaxed) {
        // Windows delivers nothing while unfocused, so the estimators simply
        // see a gap across a pause.
        if !state.capture.include_injected {
            let focused = unsafe { GetForegroundWindow() } == hwnd;
            state.capture.foreground.store(focused, Ordering::Relaxed);
        }
        state.capture.take_events(&mut events);
        model.feed(&state.capture, &events);
        model.latest = model.latest.max(state.capture.ticks_to_ns(input::now_ticks()));

        // Redraw only when something changed, plus a moment after input so the
        // live rate can fall to zero. An idle window should cost nothing.
        let foreground = state.capture.foreground.load(Ordering::Relaxed);
        if !events.is_empty() {
            active_until = Instant::now() + IDLE_REDRAW;
        }
        let clicks = std::mem::take(&mut *state.clicks.lock().unwrap());
        let mut dirty = !clicks.is_empty()
            || foreground != drawn_foreground
            || Instant::now() < active_until
            || state.repaint.swap(false, Ordering::Relaxed);
        for (x, y) in clicks {
            let Some(action) = hits.iter().find(|(r, _)| r.contains(x, y)).map(|h| h.1) else { continue };
            match action {
                Action::SelectTab(t) => tab = t,
                Action::Reset => model.reset(),
                Action::SelectDevice(handle) => match tab {
                    Tab::Mouse => model.pinned_mouse = Some(handle),
                    Tab::Keyboard => model.pinned_keyboard = Some(handle),
                },
            }
        }

        let width = state.width.load(Ordering::Relaxed);
        let height = state.height.load(Ordering::Relaxed);
        let dpi = state.dpi.load(Ordering::Relaxed);
        if width > 0 && height > 0 {
            unsafe {
                let dc = GetDC(hwnd);
                let stale = painter.as_ref().is_none_or(|p| p.width != width || p.height != height || p.dpi != dpi);
                if stale {
                    painter = Some(Painter::new(dc, width, height, dpi, theme));
                    dirty = true;
                }
                let p = painter.as_ref().unwrap();
                if dirty {
                    hits = draw(p, tab, &model, foreground);
                    drawn_foreground = foreground;
                }
                BitBlt(dc, 0, 0, width, height, p.dc, 0, 0, SRCCOPY);
                ReleaseDC(hwnd, dc);
            }
        }

        next += FRAME;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

// --------------------------------------------------------------- window

static SMALL_ICON: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(state) = STATE.get() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    match msg {
        WM_INPUT => {
            state.capture.on_wm_input(lparam);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SIZE => {
            state.width.store((lparam & 0xFFFF) as i32, Ordering::Relaxed);
            state.height.store(((lparam >> 16) & 0xFFFF) as i32, Ordering::Relaxed);
            0
        }
        WM_PAINT => {
            // The render thread repaints within a frame.
            let mut ps = zeroed();
            BeginPaint(hwnd, &mut ps);
            EndPaint(hwnd, &ps);
            state.repaint.store(true, Ordering::Relaxed);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            let x = (lparam & 0xFFFF) as i16 as i32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
            state.clicks.lock().unwrap().push((x, y));
            0
        }
        WM_GETMINMAXINFO => {
            let dpi = state.dpi.load(Ordering::Relaxed) as i32;
            let info = &mut *(lparam as *mut MINMAXINFO);
            info.ptMinTrackSize.x = MIN_WIDTH * dpi / 96;
            info.ptMinTrackSize.y = MIN_HEIGHT * dpi / 96;
            0
        }
        WM_DPICHANGED => {
            state.dpi.store((wparam & 0xFFFF) as u32, Ordering::Relaxed);
            let r = &*(lparam as *const RECT);
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        // Mashing keys must not open the system menu, which would stall the
        // message loop, or beep on Alt combinations.
        WM_SYSCOMMAND if (wparam & 0xFFF0) as u32 == SC_KEYMENU => 0,
        WM_SYSCHAR => 0,
        WM_DESTROY => {
            state.running.store(false, Ordering::Relaxed);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Draws a small bar-chart icon in memory, so the exe needs no resource file.
unsafe fn make_icon(size: i32) -> HICON {
    let screen = GetDC(null_mut());
    let color = CreateCompatibleBitmap(screen, size, size);
    let mask = CreateBitmap(size, size, 1, 1, null());
    let dc = CreateCompatibleDC(screen);
    let old = SelectObject(dc, color as HGDIOBJ);
    let brush = |c: COLORREF, x0: i32, y0: i32, x1: i32, y1: i32| {
        SetDCBrushColor(dc, c);
        let r = RECT { left: x0, top: y0, right: x1, bottom: y1 };
        FillRect(dc, &r, GetStockObject(DC_BRUSH) as HBRUSH);
    };
    brush(DARK.panel, 0, 0, size, size);
    let unit = size / 8;
    brush(DARK.accent, unit, size - 3 * unit, 3 * unit, size - unit);
    brush(DARK.accent, 3 * unit + unit / 2, size - 5 * unit, 5 * unit + unit / 2 - unit / 2, size - unit);
    brush(DARK.accent, 5 * unit + unit / 2, unit, 7 * unit, size - unit);
    SelectObject(dc, old);
    DeleteDC(dc);
    ReleaseDC(null_mut(), screen);
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(color as HGDIOBJ);
    DeleteObject(mask as HGDIOBJ);
    icon
}

pub fn run(test_mode: bool) {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);

        let state = Arc::new(WindowState {
            capture: Arc::new(Capture::new(test_mode)),
            clicks: Mutex::new(Vec::new()),
            width: AtomicI32::new(0),
            height: AtomicI32::new(0),
            dpi: AtomicU32::new(96),
            repaint: AtomicBool::new(true),
            running: AtomicBool::new(true),
        });
        let _ = STATE.set(state.clone());

        let instance = GetModuleHandleW(null());
        let class = w("PollingRateCheckerWindow");
        let big_icon = make_icon(32);
        let small_icon = make_icon(16);
        SMALL_ICON.store(small_icon, Ordering::Relaxed);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: big_icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: small_icon,
        };
        RegisterClassExW(&wc);

        let title = w("Polling Rate Checker");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1000,
            760,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return;
        }

        let dpi = GetDpiForWindow(hwnd);
        state.dpi.store(dpi, Ordering::Relaxed);
        SetWindowPos(hwnd, null_mut(), 0, 0, 1000 * dpi as i32 / 96, 760 * dpi as i32 / 96, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);

        // DWMWA_USE_IMMERSIVE_DARK_MODE
        let dark: i32 = system_uses_dark() as i32;
        DwmSetWindowAttribute(hwnd, 20, (&dark as *const i32).cast(), size_of::<i32>() as u32);

        input::register(hwnd, test_mode);

        let render_state = state.clone();
        let hwnd_value = hwnd as usize;
        let render = std::thread::spawn(move || render_loop(hwnd_value, render_state));

        ShowWindow(hwnd, SW_SHOW);
        input::pump(&state.capture);
        state.running.store(false, Ordering::Relaxed);
        let _ = render.join();
    }
}
