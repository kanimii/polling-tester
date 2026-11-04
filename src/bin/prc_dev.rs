//! Developer tool: lists devices and runs end-to-end tests by injecting input
//! at exact rates through the real Raw Input path.
//!
//!   prc-dev devices
//!   prc-dev selftest mouse <hz> <seconds>
//!   prc-dev selftest key <hz> <events>
//!   prc-dev inject mouse <hz> <seconds>
//!   prc-dev inject key <hz> <events>
//!   prc-dev record <file>
//!   prc-dev analyze <file>
//!
//! Injection is harmless: the mouse moves 1 count back and forth, and the key
//! is F24, which programs ignore.

use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use prc::device::DeviceInfo;
use prc::estimate::{KeyboardEstimator, MouseEstimator, STANDARD_RATES};
use prc::input::{self, now_ticks, Capture, InputEvent, InputKind};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, DrawTextW, EndPaint, COLOR_WINDOW, DT_NOPREFIX, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYEVENTF_KEYUP, MOUSEEVENTF_MOVE, VK_F24,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputDeviceList, RegisterRawInputDevices, RAWINPUTDEVICE, RAWINPUTDEVICELIST,
    RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static CAPTURE: OnceLock<Arc<Capture>> = OnceLock::new();
static WINDOW: AtomicUsize = AtomicUsize::new(0);

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn list_devices() {
    let mut count = 0u32;
    let entry = size_of::<RAWINPUTDEVICELIST>() as u32;
    unsafe {
        GetRawInputDeviceList(null_mut(), &mut count, entry);
        let mut list = vec![zeroed::<RAWINPUTDEVICELIST>(); count as usize];
        GetRawInputDeviceList(list.as_mut_ptr(), &mut count, entry);
        for item in &list {
            let kind = match item.dwType {
                RIM_TYPEMOUSE => "mouse",
                RIM_TYPEKEYBOARD => "keyboard",
                _ => continue,
            };
            let info = DeviceInfo::from_handle(item.hDevice as usize);
            let endpoint = info.endpoint.map_or("no descriptor".to_string(), |e| {
                format!("descriptor {:.0} Hz ({} us, {})", e.rate_hz(), e.interval_us, e.speed.label())
            });
            let ids = info.vid_pid.map_or("----:----".to_string(), |(v, p)| format!("{v:04X}:{p:04X}"));
            println!("{kind:<9} {ids}  {:?}  {endpoint}  {}", info.connection, info.name);
        }
    }
}

fn spin_until(ticks: i64) {
    while now_ticks() < ticks {
        std::hint::spin_loop();
    }
}

fn send_mouse(dx: i32) {
    let mut event: INPUT = unsafe { zeroed() };
    event.r#type = INPUT_MOUSE;
    event.Anonymous.mi.dx = dx;
    event.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
    unsafe { SendInput(1, &event, size_of::<INPUT>() as i32) };
}

fn send_key(up: bool) {
    let mut event: INPUT = unsafe { zeroed() };
    event.r#type = INPUT_KEYBOARD;
    event.Anonymous.ki.wVk = VK_F24;
    event.Anonymous.ki.dwFlags = if up { KEYEVENTF_KEYUP } else { 0 };
    unsafe { SendInput(1, &event, size_of::<INPUT>() as i32) };
}

fn freq() -> i64 {
    let mut f = 0;
    unsafe { QueryPerformanceFrequency(&mut f) };
    f
}

fn inject_mouse(hz: f64, secs: f64) {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    let period = freq() as f64 / hz;
    let start = now_ticks();
    let end = start + (secs * freq() as f64) as i64;
    let mut dx = 1;
    let mut slot = 0u64;
    loop {
        spin_until(start + (slot as f64 * period) as i64);
        send_mouse(dx);
        dx = -dx;
        // Like hardware, a missed poll is skipped rather than replayed in a burst.
        let now = now_ticks();
        if now >= end {
            break;
        }
        slot = (slot + 1).max(((now - start) as f64 / period) as u64 + 1);
    }
}

/// Sends F24 changes on a grid of `hz`, with human-like random gaps of 4 to 45 ms.
fn inject_keys(hz: f64, events: u64) {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    let period = freq() as f64 / hz;
    let start = now_ticks();
    let mut slot = 0u64;
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut up = false;
    for _ in 0..events {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let wait_ms = 4.0 + (seed % 410) as f64 / 10.0;
        slot += ((wait_ms * hz / 1000.0) as u64).max(1);
        spin_until(start + (slot as f64 * period) as i64);
        send_key(up);
        up = !up;
    }
    if up {
        send_key(true);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            if let Some(capture) = CAPTURE.get() {
                capture.on_wm_input(lparam);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            let mut ps = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut text: Vec<u16> = RECORD_HELP.encode_utf16().collect();
            let mut rect = RECT { left: 16, top: 16, right: 700, bottom: 300 };
            DrawTextW(dc, text.as_mut_ptr(), text.len() as i32, &mut rect, DT_NOPREFIX);
            EndPaint(hwnd, &ps);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Injects input while capturing it in a foreground window, then prints what
/// the estimators made of it.
fn selftest(kind: &str, hz: f64, amount: f64) {
    unsafe {
        let capture = Arc::new(Capture::new(true));
        let _ = CAPTURE.set(capture.clone());
        let class = w("PrcSelfTest");
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let title = w("prc-dev self test");
        // A hidden window with an input sink, so the test never takes focus from
        // whoever is using the PC. Only the app itself must stay foreground-only.
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), title.as_ptr(), WS_POPUP,
            0, 0, 40, 40, null_mut(), null_mut(), instance, null(),
        );
        WINDOW.store(hwnd as usize, Ordering::Relaxed);
        let device = |usage: u16| RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: usage,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        let devices = [device(0x02), device(0x06)];
        RegisterRawInputDevices(devices.as_ptr(), 2, size_of::<RAWINPUTDEVICE>() as u32);

        let kind_owned = kind.to_string();
        let injector = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let foreground = GetForegroundWindow() as usize == WINDOW.load(Ordering::Relaxed);
            if kind_owned == "mouse" {
                inject_mouse(hz, amount);
            } else {
                inject_keys(hz, amount as u64);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            PostMessageW(WINDOW.load(Ordering::Relaxed) as HWND, WM_CLOSE, 0, 0);
            foreground
        });

        input::pump(&capture);
        let foreground = injector.join().unwrap();

        let mut events = Vec::new();
        capture.take_events(&mut events);
        // Injected input has no device handle; anything else is a person using the PC.
        events.retain(|e| e.device == 0);
        let mut mouse = MouseEstimator::new();
        let mut keys = KeyboardEstimator::new();
        let mut last = 0;
        for event in &events {
            let t = capture.ticks_to_ns(event.ticks);
            last = t;
            match event.kind {
                InputKind::Mouse => mouse.push(t),
                InputKind::Key { code, down } => keys.push(t, code, down),
            }
        }
        let span = events.last().map_or(0.0, |e| capture.ticks_to_ns(e.ticks) as f64) - events.first().map_or(0.0, |e| capture.ticks_to_ns(e.ticks) as f64);
        let actual = if span > 0.0 { (events.len() - 1) as f64 * 1e9 / span } else { 0.0 };
        println!("foreground={foreground} events={} actual_delivered_hz={actual:.0}", events.len());
        if kind == "mouse" {
            let r = mouse.report(last);
            println!(
                "injected={hz} measured={:?} snapped={:?} quality={:?} steady={:.2} median_us={:?} p99_us={:?}",
                r.measured_hz.map(|v| (v * 10.0).round() / 10.0), r.snapped, r.quality, r.steady_fraction, r.median_interval_us, r.p99_interval_us
            );
        } else {
            let r = keys.report();
            let scores: Vec<String> = r.scores.iter().map(|s| format!("{s:.2}")).collect();
            println!(
                "injected={hz} result={:?} gaps={} threshold={:.2} scores(125..8000)={}",
                r.result, r.gaps, r.threshold, scores.join(" ")
            );
        }
    }
}

const RECORD_HELP: &str = "Recording timing only: never which keys you press. Keep this window focused.\n\n\
1. Set the LAMZU to 8K, then move it fast in large circles for 10 seconds.\n\
2. On the WIN 60 HE, roll your fingers across letter keys for 30 seconds.\n\
3. On the TH80, do the same for 30 seconds.\n\
4. Close this window to finish.\n\n\
Avoid the Windows and Alt keys, which move focus away.";

/// Records real input timing in a focused window, the same way the app does.
fn record(out: &str) {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        let capture = Arc::new(Capture::new(false));
        let _ = CAPTURE.set(capture.clone());
        let class = w("PrcRecorder");
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let title = w("Polling Rate Checker: timing recorder");
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT, CW_USEDEFAULT, 760, 260, null_mut(), null_mut(), instance, null(),
        );
        SetForegroundWindow(hwnd);
        input::register(hwnd, false);
        input::pump(&capture);

        let mut events = Vec::new();
        capture.take_events(&mut events);
        write_recording(out, &capture, &events);
    }
    analyze(out);
}

/// Writes one row per report. Key identity stays in memory, used only to drop
/// auto-repeat and to note whether a change touched the same key as the last.
fn write_recording(out: &str, capture: &Capture, events: &[InputEvent]) {
    let mut handles: Vec<usize> = Vec::new();
    let mut held: HashMap<usize, HashSet<u32>> = HashMap::new();
    let mut last_key: HashMap<usize, u32> = HashMap::new();
    let mut rows = String::new();
    for event in events {
        let index = match handles.iter().position(|&h| h == event.device) {
            Some(i) => i,
            None => {
                handles.push(event.device);
                handles.len() - 1
            }
        };
        let t = capture.ticks_to_ns(event.ticks);
        match event.kind {
            InputKind::Mouse => rows.push_str(&format!("{t},{index},m,0,0,0\n")),
            InputKind::Key { code, down } => {
                let set = held.entry(index).or_default();
                let fresh = if down { set.insert(code) } else { set.remove(&code); true };
                if !fresh {
                    continue;
                }
                let same = last_key.insert(index, code) == Some(code);
                rows.push_str(&format!("{t},{index},k,{},{},{}\n", down as u8, set.len(), same as u8));
            }
        }
    }
    let mut text = String::new();
    for (index, &handle) in handles.iter().enumerate() {
        let info = DeviceInfo::from_handle(handle);
        text.push_str(&format!("# {index} {}\n", info.name));
    }
    text.push_str(&rows);
    std::fs::write(out, text).expect("could not write the recording");
}

fn period_score(gaps: &[u64], period_ns: f64) -> f64 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for &gap in gaps {
        let phase = std::f64::consts::TAU * gap as f64 / period_ns;
        re += phase.cos();
        im += phase.sin();
    }
    if gaps.is_empty() { 0.0 } else { (re * re + im * im).sqrt() / gaps.len() as f64 }
}

fn print_scores(label: &str, gaps: &[u64]) {
    let scores: Vec<String> = STANDARD_RATES
        .iter()
        .map(|&r| format!("{r}:{:.2}", period_score(gaps, 1e9 / r as f64)))
        .collect();
    println!("  {label} (n={}): {}", gaps.len(), scores.join(" "));
}

fn print_histogram(label: &str, gaps: &[u64], bin_us: u64, max_us: u64) {
    let mut bins = vec![0u32; (max_us / bin_us) as usize];
    for &gap in gaps {
        if let Some(bin) = bins.get_mut((gap / 1000 / bin_us) as usize) {
            *bin += 1;
        }
    }
    let cells: Vec<String> = bins
        .iter()
        .enumerate()
        .filter(|(_, &c)| c > 0)
        .map(|(i, c)| format!("{}:{c}", i as u64 * bin_us))
        .collect();
    println!("  {label}, us bin:count = {}", cells.join(" "));
}

/// Strongest grids across a fine sweep of periods, to catch non-standard ones.
fn print_sweep(gaps: &[u64]) {
    let periods: Vec<f64> = (100..=2100).step_by(5).map(|us| us as f64 * 1000.0).collect();
    let scores: Vec<f64> = periods.iter().map(|&p| period_score(gaps, p)).collect();
    let mut peaks: Vec<(f64, f64)> = Vec::new();
    for (i, window) in scores.windows(3).enumerate() {
        if window[1] >= window[0] && window[1] >= window[2] {
            peaks.push((periods.iter().copied().nth(i + 1).unwrap(), window[1]));
        }
    }
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    let cells: Vec<String> = peaks.iter().take(6).map(|(p, s)| format!("{:.0}us:{s:.2}", p / 1000.0)).collect();
    println!("  strongest periods: {}", cells.join(" "));
}

fn analyze(path: &str) {
    let text = std::fs::read_to_string(path).expect("could not read the recording");
    let mut names: HashMap<usize, String> = HashMap::new();
    let mut rows: HashMap<(usize, char), Vec<(u64, bool, u32, bool)>> = HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if let Some((index, name)) = rest.split_once(' ') {
                names.insert(index.parse().unwrap(), name.to_string());
            }
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        if f.len() == 6 {
            let row = (f[0].parse().unwrap(), f[3] == "1", f[4].parse().unwrap(), f[5] == "1");
            rows.entry((f[1].parse().unwrap(), f[2].chars().next().unwrap())).or_default().push(row);
        }
    }
    let mut keys: Vec<_> = rows.keys().copied().collect();
    keys.sort();
    for key in keys {
        let list = &rows[&key];
        let name = names.get(&key.0).map_or("?", String::as_str);
        let gaps: Vec<u64> = list.windows(2).map(|p| p[1].0.saturating_sub(p[0].0)).filter(|&g| g <= 200_000_000).collect();
        if key.1 == 'm' {
            let mut est = MouseEstimator::new();
            list.iter().for_each(|r| est.push(r.0));
            let report = est.report(list.last().map_or(0, |r| r.0));
            println!("MOUSE {name}: reports={} measured={:?} snapped={:?} quality={:?} steady={:.2} median_us={:?}",
                list.len(), report.measured_hz.map(|v| v.round()), report.snapped, report.quality, report.steady_fraction, report.median_interval_us);
            print_histogram("intervals", &gaps, 25, 1000);
            print_scores("scores", &gaps);
            print_sweep(&gaps);
        } else {
            // Replays through the app's estimator. The recording holds no key
            // identity, so each press gets a fresh number.
            let mut est = KeyboardEstimator::new();
            for (i, r) in list.iter().enumerate() {
                est.push(r.0, i as u32, r.1);
            }
            let app = est.report();
            println!("KEYBOARD {name}: changes={} gaps={}", list.len(), gaps.len());
            println!("  app reads: {:?} scored_gaps={} threshold={:.2}", app.result, app.gaps, app.threshold);
            print_histogram("gaps", &gaps, 50, 2500);
            let short: Vec<u64> = gaps.iter().copied().filter(|&g| g < 1_500_000).collect();
            let long: Vec<u64> = gaps.iter().copied().filter(|&g| g >= 1_500_000).collect();
            print_scores("all gaps", &gaps);
            print_scores("gaps from 1.5 ms", &long);
            print_scores("gaps under 1.5 ms", &short);
            print_sweep(&long);
            // What was happening at each short gap: dd = press then press, and so on.
            let mut kinds: HashMap<&str, u32> = HashMap::new();
            let mut same_key = 0;
            let mut chorded = 0;
            for p in list.windows(2) {
                if p[1].0.saturating_sub(p[0].0) >= 1_500_000 {
                    continue;
                }
                let kind = match (p[0].1, p[1].1) {
                    (true, true) => "dd",
                    (true, false) => "du",
                    (false, true) => "ud",
                    (false, false) => "uu",
                };
                *kinds.entry(kind).or_default() += 1;
                same_key += p[1].3 as u32;
                chorded += (p[1].2 >= 2) as u32;
            }
            println!("  short gap kinds: {kinds:?} same_key={same_key} two_or_more_held={chorded}");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let num = |s: &str| s.parse::<f64>().expect("expected a number");
    match args.as_slice() {
        ["devices"] => list_devices(),
        ["record", out] => record(out),
        ["analyze", path] => analyze(path),
        ["selftest", kind @ ("mouse" | "key"), hz, amount] => selftest(kind, num(hz), num(amount)),
        ["inject", "mouse", hz, secs] => inject_mouse(num(hz), num(secs)),
        ["inject", "key", hz, events] => inject_keys(num(hz), num(events) as u64),
        _ => eprintln!("usage: prc-dev devices | selftest mouse|key <hz> <secs|events> | inject mouse|key <hz> <secs|events>"),
    }
}
