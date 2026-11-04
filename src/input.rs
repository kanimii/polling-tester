//! Raw Input capture. Runs on the thread that owns the window, so timestamps
//! are taken before any drawing can delay them.

use std::mem::{size_of, MaybeUninit};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

const RI_KEY_BREAK: u16 = 1;
const RI_KEY_E0: u16 = 2;
const RI_KEY_E1: u16 = 4;

/// How long the pump keeps spinning on the queue after the last report.
const SPIN_AFTER_INPUT_MS: i64 = 1500;

#[derive(Clone, Copy, Debug)]
pub enum InputKind {
    Mouse,
    Key { code: u32, down: bool },
}

#[derive(Clone, Copy, Debug)]
pub struct InputEvent {
    pub ticks: i64,
    /// Raw Input device handle; 0 for injected input.
    pub device: usize,
    pub kind: InputKind,
}

/// State shared between the input thread and its consumer.
pub struct Capture {
    pub events: Mutex<Vec<InputEvent>>,
    pub include_injected: bool,
    pub foreground: AtomicBool,
    last_input: AtomicI64,
    freq: i64,
}

pub fn now_ticks() -> i64 {
    let mut ticks = 0;
    unsafe { QueryPerformanceCounter(&mut ticks) };
    ticks
}

impl Capture {
    pub fn new(include_injected: bool) -> Self {
        let mut freq = 0;
        unsafe { QueryPerformanceFrequency(&mut freq) };
        Self {
            events: Mutex::new(Vec::with_capacity(4096)),
            include_injected,
            foreground: AtomicBool::new(true),
            last_input: AtomicI64::new(0),
            freq,
        }
    }

    pub fn ticks_to_ns(&self, ticks: i64) -> u64 {
        (ticks as i128 * 1_000_000_000 / self.freq as i128) as u64
    }

    pub fn take_events(&self, into: &mut Vec<InputEvent>) {
        into.clear();
        std::mem::swap(&mut *self.events.lock().unwrap(), into);
    }

    /// Handles one WM_INPUT message. The timestamp is the first thing taken.
    pub fn on_wm_input(&self, lparam: LPARAM) {
        let ticks = now_ticks();
        let mut raw = MaybeUninit::<RAWINPUT>::uninit();
        let mut size = size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                raw.as_mut_ptr().cast(),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX || read == 0 {
            return;
        }
        let raw = unsafe { raw.assume_init() };
        let device = raw.header.hDevice as usize;
        if device == 0 && !self.include_injected {
            return;
        }
        let kind = match raw.header.dwType {
            RIM_TYPEMOUSE => {
                let mouse = unsafe { raw.data.mouse };
                // Button-only reports say nothing about motion rate.
                if mouse.lLastX == 0 && mouse.lLastY == 0 {
                    return;
                }
                InputKind::Mouse
            }
            RIM_TYPEKEYBOARD => {
                let key = unsafe { raw.data.keyboard };
                let mut code = key.MakeCode as u32;
                if key.Flags & RI_KEY_E0 != 0 {
                    code |= 0xE000;
                }
                if key.Flags & RI_KEY_E1 != 0 {
                    code |= 0xE100;
                }
                InputKind::Key { code, down: key.Flags & RI_KEY_BREAK == 0 }
            }
            _ => return,
        };
        self.last_input.store(ticks, Ordering::Relaxed);
        self.events.lock().unwrap().push(InputEvent { ticks, device, kind });
    }

    fn should_spin(&self) -> bool {
        let last = self.last_input.load(Ordering::Relaxed);
        self.foreground.load(Ordering::Relaxed)
            && last != 0
            && (now_ticks() - last) * 1000 / self.freq < SPIN_AFTER_INPUT_MS
    }
}

/// Registers mouse and keyboard Raw Input on `hwnd`. There is deliberately no
/// RIDEV_INPUTSINK: Windows then delivers nothing while another program has
/// focus, which avoids the background rate cap and any view of typing elsewhere.
/// `test_mode` adds the sink for developer UI tests only.
pub fn register(hwnd: HWND, test_mode: bool) -> bool {
    let device = |usage: u16| RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: usage,
        dwFlags: if test_mode { RIDEV_INPUTSINK } else { 0 },
        hwndTarget: hwnd,
    };
    let devices = [device(0x02), device(0x06)];
    unsafe {
        RegisterRawInputDevices(devices.as_ptr(), 2, size_of::<RAWINPUTDEVICE>() as u32) != 0
    }
}

/// Runs the message loop. While input is arriving it polls the queue instead of
/// blocking, because a blocked thread adds scheduler wake-up jitter to every
/// timestamp, and that jitter is what hides fine keyboard grids.
pub fn pump(capture: &Capture) {
    let mut msg = MaybeUninit::<MSG>::zeroed();
    loop {
        let got = if capture.should_spin() {
            let got = unsafe { PeekMessageW(msg.as_mut_ptr(), std::ptr::null_mut(), 0, 0, PM_REMOVE) };
            if got == 0 {
                std::hint::spin_loop();
                continue;
            }
            if unsafe { msg.assume_init_ref() }.message == WM_QUIT {
                return;
            }
            true
        } else {
            unsafe { GetMessageW(msg.as_mut_ptr(), std::ptr::null_mut(), 0, 0) > 0 }
        };
        if !got {
            return;
        }
        unsafe {
            TranslateMessage(msg.as_ptr());
            DispatchMessageW(msg.as_ptr());
        }
    }
}
