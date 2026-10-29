//! kvm-switch guest agent for Windows: pushing the pointer `threshold`
//! mickeys past the left edge of the desktop asks kvm-switch-router on the
//! host (vsock CID 2) to take the keyboard and mouse back. When the router
//! hands them over, the pointer moves to the left edge at the height it left
//! the host at.
//!
//!   kvm-switch-guest.exe [port [threshold]]     (defaults 8788, 50)
//!
//! Runs in the user's session (a logon task). The logon screen and UAC
//! prompts are on the secure desktop, where only the Alt+Alt hotkey works.
//!
//! The clipboard follows the input through clipboard-bridge, whose own guest
//! agent connects to the next port.
//!
//! Needs the viosock driver, which registers AF_VSOCK (40) as a Winsock
//! provider. It supports only blocking calls, so the connection lives on its
//! own thread; raw input arrives on a message-only window.

#![windows_subsystem = "windows"]

use std::{
    cell::Cell,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Networking::WinSock::{
        INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKET, WSADATA, WSAStartup, closesocket, connect,
        recv, send, socket,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::{
            GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
            RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
            GetSystemMetrics, HWND_MESSAGE, MSG, RegisterClassW, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos, SetProcessDPIAware, WM_INPUT,
            WNDCLASSW,
        },
    },
};

const AF_VSOCK: u16 = 40;
const VMADDR_CID_HOST: u32 = 2;

/// struct sockaddr_vm
#[repr(C)]
struct SockaddrVm {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    zero: [u8; 4],
}

/// The connection to the router, shared with the GUI thread for sending.
static CONN: Mutex<Option<SOCKET>> = Mutex::new(None);
/// Whether the router currently sends the input here.
static ACTIVE: AtomicBool = AtomicBool::new(false);
thread_local! {
    static PUSH: Cell<i64> = const { Cell::new(0) };
    static THRESHOLD: Cell<i64> = const { Cell::new(50) };
}

fn send_line(line: &str) {
    if let Some(s) = *CONN.lock().unwrap() {
        unsafe { send(s, line.as_ptr(), line.len() as i32, 0) };
    }
}

fn metrics() -> (i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn handle_line(line: &str) {
    let mut words = line.split_whitespace();
    match words.next() {
        Some("enter") => {
            if let Some(y) = words
                .next()
                .and_then(|y| y.parse::<f64>().ok())
                .filter(|y| (0.0..=1.0).contains(y))
            {
                let (vx, vy, vh) = metrics();
                unsafe { SetCursorPos(vx, vy + (y * f64::from(vh - 1)) as i32) };
            }
            ACTIVE.store(true, Ordering::SeqCst);
        }
        Some("leave") => ACTIVE.store(false, Ordering::SeqCst),
        _ => {}
    }
}

/// Connects to `port` on the host and runs `serve` on the socket, again
/// whenever it ends.
fn reconnect(port: u32, serve: fn(SOCKET)) -> ! {
    loop {
        let s = unsafe { socket(i32::from(AF_VSOCK), SOCK_STREAM, 0) };
        if s != INVALID_SOCKET {
            let addr = SockaddrVm {
                family: AF_VSOCK,
                reserved: 0,
                port,
                cid: VMADDR_CID_HOST,
                zero: [0; 4],
            };
            let ok = unsafe {
                connect(
                    s,
                    (&addr as *const SockaddrVm).cast::<SOCKADDR>(),
                    size_of::<SockaddrVm>() as i32,
                )
            } == 0;
            if ok {
                serve(s);
            }
            unsafe { closesocket(s) };
        }
        thread::sleep(Duration::from_secs(3));
    }
}

/// Reads the router's lines.
fn router(s: SOCKET) {
    *CONN.lock().unwrap() = Some(s);
    let mut pending = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe { recv(s, buf.as_mut_ptr(), buf.len() as i32, 0) };
        if n <= 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n as usize]);
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            handle_line(&String::from_utf8_lossy(&line));
        }
        if pending.len() > 4096 {
            pending.clear();
        }
    }
    *CONN.lock().unwrap() = None;
    ACTIVE.store(false, Ordering::SeqCst);
}

fn on_motion(dx: i32) {
    let mut pt = unsafe { zeroed() };
    let (vx, vy, vh) = metrics();
    if !ACTIVE.load(Ordering::SeqCst)
        || unsafe { GetCursorPos(&mut pt) } == 0
        || pt.x > vx
        || dx > 0
    {
        PUSH.set(0);
        return;
    }
    let push = PUSH.get() - i64::from(dx);
    if push < THRESHOLD.get() {
        PUSH.set(push);
        return;
    }
    PUSH.set(0);
    let y = if vh > 1 {
        (f64::from(pt.y - vy) / f64::from(vh - 1)).clamp(0.0, 1.0)
    } else {
        0.5
    };
    ACTIVE.store(false, Ordering::SeqCst);
    send_line(&format!("host {y:.4}\n"));
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        let mut ri: RAWINPUT = unsafe { zeroed() };
        let mut size = size_of::<RAWINPUT>() as u32;
        let got = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                (&mut ri as *mut RAWINPUT).cast(),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if got != u32::MAX && ri.header.dwType == RIM_TYPEMOUSE {
            let mouse = unsafe { ri.data.mouse };
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 && mouse.lLastX != 0 {
                on_motion(mouse.lLastX);
            }
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let port = args.next().and_then(|p| p.parse().ok()).unwrap_or(8788);
    if let Some(t) = args
        .next()
        .and_then(|t| t.parse().ok())
        .filter(|&t: &i64| t > 0)
    {
        THRESHOLD.set(t);
    }

    unsafe {
        // Real pixels for the cursor position and the screen metrics.
        SetProcessDPIAware();
        let mut wsa: WSADATA = zeroed();
        if WSAStartup(0x0202, &mut wsa) != 0 {
            return;
        }
        let class: Vec<u16> = "kvm-switch-guest\0".encode_utf16().collect();
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        let rid = RAWINPUTDEVICE {
            usUsagePage: 0x01, // generic desktop
            usUsage: 0x02,     // mouse
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        if RegisterRawInputDevices(&rid, 1, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            return;
        }
        thread::spawn(move || reconnect(port, router));
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
