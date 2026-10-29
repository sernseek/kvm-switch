//! Grabs the physical keyboards and mice and replays them on the host or the
//! guest pair of uinput devices (see the crate docs).
//!
//!   kvm-switch-router [--vsock-port N] [--guest-cid N] [--socket-gid N]
//!
//! Control socket (`socket_path()`, line based) for kvm-switch-edge:
//!   -> `available 0|1`  whether something (QEMU) reads the guest devices
//!   -> `target host|guest`
//!   <- `guest [Y]`, `host`, `toggle`  (Y: vertical position, 0..1)
//! vsock, from the guest agent:
//!   -> `enter [Y]`, `leave`
//!   <- `host [Y]`
//!
//! The touchpad is read all the time but grabbed only while the guest has
//! the input; `kvm_switch::touchpad` turns it into the guest's mouse.

use anyhow::{Context, Result};
use evdev::{
    AttributeSet, BusType, InputEvent, InputId, KeyCode, RelativeAxisCode, raw_stream::RawDevice,
    uinput::VirtualDevice,
};
use kvm_switch::{
    Action, EV_KEY, Ev, GuestPointer, Output, Router, Target, parse_fraction, socket_path,
    touchpad::{EV_ABS, TouchOut, Touchpad},
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::{
            fs::PermissionsExt,
            net::{SocketAddr, UnixDatagram, UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

const KEY_A: u16 = 30;
const BTN_LEFT: u16 = 0x110;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const INPUT_PROP_POINTER: u16 = 0x00;
/// _IOW('E', 0x90, int)
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

enum Msg {
    Frame(Vec<Ev>),
    DeviceGone(PathBuf),
    /// A second descriptor for grabbing it, and its state machine.
    Touchpad(OwnedFd, Touchpad),
    TouchFrame(Vec<Ev>, u64),
    TouchpadGone,
    Client(u64, UnixStream),
    ClientLine(String),
    ClientGone(u64),
    Agent(u64, vsock::VsockStream),
    AgentLine(String),
    AgentGone(u64),
    Tick,
}

struct Args {
    vsock_port: u32,
    guest_cid: u32,
    socket_gid: Option<u32>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        vsock_port: 8788,
        guest_cid: 3,
        socket_gid: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--vsock-port" => args.vsock_port = value.parse()?,
            "--guest-cid" => args.guest_cid = value.parse()?,
            "--socket-gid" => args.socket_gid = Some(value.parse()?),
            _ => anyhow::bail!("unknown argument {flag}"),
        }
    }
    Ok(args)
}

/// The keyboard and mouse a target sees.
struct Pair {
    kbd: VirtualDevice,
    mouse: VirtualDevice,
}

impl Pair {
    fn new(role: &str) -> Result<Pair> {
        let id = InputId::new(BusType::BUS_VIRTUAL, 0x1209, 0x6b76, 1);
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in (1..0x100).chain(0x160..0x2ff) {
            keys.insert(KeyCode(code));
        }
        let kbd = VirtualDevice::builder()?
            .name(&format!("kvm-switch {role} keyboard"))
            .input_id(id.clone())
            .with_keys(&keys)?
            .build()?;
        let mut buttons = AttributeSet::<KeyCode>::new();
        for code in BTN_LEFT..=0x117 {
            buttons.insert(KeyCode(code));
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for axis in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            axes.insert(axis);
        }
        let mouse = VirtualDevice::builder()?
            .name(&format!("kvm-switch {role} mouse"))
            .input_id(id)
            .with_keys(&buttons)?
            .with_relative_axes(&axes)?
            .build()?;
        Ok(Pair { kbd, mouse })
    }

    fn emit(&mut self, events: &[(Output, Ev)]) {
        for (output, dev) in [
            (Output::Keyboard, &mut self.kbd),
            (Output::Mouse, &mut self.mouse),
        ] {
            let batch: Vec<InputEvent> = events
                .iter()
                .filter(|(o, _)| *o == output)
                .map(|(_, e)| InputEvent::new(e.kind, e.code, e.value))
                .collect();
            if !batch.is_empty()
                && let Err(e) = dev.emit(&batch)
            {
                eprintln!("kvm-switch: write to uinput failed: {e}");
            }
        }
    }

    fn nodes(&mut self) -> Result<Vec<PathBuf>> {
        let mut nodes = Vec::new();
        for dev in [&mut self.kbd, &mut self.mouse] {
            for node in dev.enumerate_dev_nodes_blocking()? {
                nodes.push(node?);
            }
        }
        Ok(nodes)
    }
}

/// `/dev/input/eventN` nodes of physical keyboards and mice, deduplicated.
fn candidates() -> HashSet<PathBuf> {
    let mut found = HashSet::new();
    for dir in ["/dev/input/by-id", "/dev/input/by-path"] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if (name.ends_with("-event-kbd") || name.ends_with("-event-mouse"))
                && let Ok(node) = fs::canonicalize(entry.path())
            {
                found.insert(node);
            }
        }
    }
    found
}

/// Keyboards and relative mice; touchpads (absolute) stay with the host.
fn usable(dev: &RawDevice) -> bool {
    let keys = dev.supported_keys();
    let has = |code| keys.is_some_and(|k| k.contains(KeyCode(code)));
    let rel_x = dev
        .supported_relative_axes()
        .is_some_and(|a| a.contains(RelativeAxisCode::REL_X));
    has(KEY_A) || (rel_x && has(BTN_LEFT))
}

/// Reads one physical device until it disappears, one SYN frame per message.
fn watch(path: PathBuf, tx: Sender<Msg>) {
    let mut dev = match RawDevice::open(&path) {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("kvm-switch: open {}: {e}", path.display());
            return;
        }
    };
    if !usable(&dev) {
        return;
    }
    let name = dev.name().unwrap_or("?").to_owned();
    // A key held while grabbing would never be released on whoever had it.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && dev.get_key_state().is_ok_and(|k| k.iter().next().is_some())
    {
        thread::sleep(Duration::from_millis(20));
    }
    if let Err(e) = dev.grab() {
        eprintln!("kvm-switch: grab {} ({name}): {e}", path.display());
        let _ = tx.send(Msg::DeviceGone(path));
        return;
    }
    eprintln!("kvm-switch: grabbed {} ({name})", path.display());
    let mut frame = Vec::new();
    loop {
        match dev.fetch_events() {
            Ok(events) => {
                for e in events {
                    let ev = Ev {
                        kind: e.event_type().0,
                        code: e.code(),
                        value: e.value(),
                    };
                    match (ev.kind, ev.code) {
                        (0, SYN_REPORT) if !frame.is_empty() => {
                            if tx.send(Msg::Frame(std::mem::take(&mut frame))).is_err() {
                                return;
                            }
                        }
                        (0, SYN_DROPPED) => frame.clear(),
                        (0, _) => {}
                        _ => frame.push(ev),
                    }
                }
            }
            Err(e) => {
                eprintln!("kvm-switch: {} ({name}) gone: {e}", path.display());
                let _ = tx.send(Msg::DeviceGone(path));
                return;
            }
        }
    }
}

/// A multitouch pointer device: what libinput treats as a touchpad.
fn is_touchpad(dev: &RawDevice) -> bool {
    let mt = dev
        .supported_absolute_axes()
        .is_some_and(|a| a.iter().any(|c| c.0 == ABS_MT_POSITION_X));
    let pointer = dev.properties().iter().any(|p| p.0 == INPUT_PROP_POINTER);
    mt && pointer
}

/// Finds the touchpad and reads it until it disappears (not grabbed: the
/// main thread grabs it through the descriptor it is sent).
fn watch_touchpad(tx: Sender<Msg>) {
    let found = fs::read_dir("/dev/input")
        .into_iter()
        .flatten()
        .flatten()
        .find_map(|e| {
            if !e.file_name().to_string_lossy().starts_with("event") {
                return None;
            }
            let dev = RawDevice::open(e.path()).ok()?;
            is_touchpad(&dev).then_some((e.path(), dev))
        });
    let Some((path, mut dev)) = found else {
        thread::sleep(Duration::from_secs(5));
        let _ = tx.send(Msg::TouchpadGone);
        return;
    };
    let mut res = (0.0, 0.0);
    let mut slots = 1;
    for (axis, info) in dev.get_absinfo().into_iter().flatten() {
        match axis.0 {
            ABS_MT_POSITION_X => res.0 = f64::from(info.resolution()),
            ABS_MT_POSITION_Y => res.1 = f64::from(info.resolution()),
            ABS_MT_SLOT => slots = info.maximum().max(0) as usize + 1,
            _ => {}
        }
    }
    let grab = unsafe { libc::dup(dev.as_raw_fd()) };
    if grab < 0 {
        return;
    }
    let grab = unsafe { OwnedFd::from_raw_fd(grab) };
    eprintln!(
        "kvm-switch: touchpad {} ({}), {res:?} units/mm, {slots} slots",
        path.display(),
        dev.name().unwrap_or("?")
    );
    if tx
        .send(Msg::Touchpad(grab, Touchpad::new(res.0, res.1, slots)))
        .is_err()
    {
        return;
    }
    let mut frame = Vec::new();
    loop {
        let Ok(events) = dev.fetch_events() else {
            let _ = tx.send(Msg::TouchpadGone);
            return;
        };
        for e in events {
            let ev = Ev {
                kind: e.event_type().0,
                code: e.code(),
                value: e.value(),
            };
            match (ev.kind, ev.code) {
                (0, SYN_REPORT) => {
                    let ms = e
                        .timestamp()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis() as u64);
                    if tx
                        .send(Msg::TouchFrame(std::mem::take(&mut frame), ms))
                        .is_err()
                    {
                        return;
                    }
                }
                (0, SYN_DROPPED) => frame.clear(),
                (EV_ABS | EV_KEY, _) => frame.push(ev),
                _ => {}
            }
        }
    }
}

/// Whether a QEMU process holds one of `nodes` open.
fn consumed(nodes: &[PathBuf]) -> bool {
    let Ok(procs) = fs::read_dir("/proc") else {
        return false;
    };
    for proc in procs.flatten() {
        let dir = proc.path();
        let is_qemu = fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.contains("qemu"));
        if !is_qemu {
            continue;
        }
        let Ok(fds) = fs::read_dir(dir.join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if fs::read_link(fd.path()).is_ok_and(|t| nodes.contains(&t)) {
                return true;
            }
        }
    }
    false
}

fn read_lines<R: std::io::Read + Send + 'static>(
    reader: R,
    tx: Sender<Msg>,
    line: impl Fn(String) -> Msg + Send + 'static,
    gone: Msg,
) {
    thread::spawn(move || {
        for l in BufReader::new(reader).lines() {
            let Ok(l) = l else { break };
            if tx.send(line(l)).is_err() {
                return;
            }
        }
        let _ = tx.send(gone);
    });
}

fn listen_control(gid: Option<u32>, tx: Sender<Msg>) -> Result<()> {
    let socket = socket_path();
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("bind {socket}"))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = gid {
        std::os::unix::fs::chown(&socket, None, Some(gid))?;
    }
    thread::spawn(move || {
        for (id, stream) in (1u64..).zip(listener.incoming().flatten()) {
            let Ok(reader) = stream.try_clone() else {
                continue;
            };
            read_lines(reader, tx.clone(), Msg::ClientLine, Msg::ClientGone(id));
            if tx.send(Msg::Client(id, stream)).is_err() {
                return;
            }
        }
    });
    Ok(())
}

fn listen_vsock(port: u32, guest_cid: u32, tx: Sender<Msg>) -> Result<()> {
    let listener = vsock::VsockListener::bind_with_cid_port(vsock::VMADDR_CID_ANY, port)
        .with_context(|| format!("bind vsock port {port}"))?;
    thread::spawn(move || {
        let mut id = 0u64;
        loop {
            let Ok((stream, addr)) = listener.accept() else {
                continue;
            };
            if addr.cid() != guest_cid {
                eprintln!("kvm-switch: rejected vsock peer cid {}", addr.cid());
                continue;
            }
            id += 1;
            let Ok(reader) = stream.try_clone() else {
                continue;
            };
            let agent = id;
            read_lines(reader, tx.clone(), Msg::AgentLine, Msg::AgentGone(agent));
            if tx.send(Msg::Agent(agent, stream)).is_err() {
                return;
            }
        }
    });
    Ok(())
}

fn notify_ready() {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let addr = match path.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(&path),
    };
    if let (Ok(sock), Ok(addr)) = (UnixDatagram::unbound(), addr) {
        let _ = sock.send_to_addr(b"READY=1", &addr);
    }
}

struct State {
    router: Router,
    host: Pair,
    guest: Pair,
    guest_nodes: Vec<PathBuf>,
    available: bool,
    clients: HashMap<u64, UnixStream>,
    agent: Option<(u64, vsock::VsockStream)>,
    touchpad: Option<(OwnedFd, Touchpad)>,
    watching_touchpad: bool,
    pointer: GuestPointer,
    known: HashSet<PathBuf>,
    tx: Sender<Msg>,
}

impl State {
    fn pair(&mut self, target: Target) -> &mut Pair {
        match target {
            Target::Host => &mut self.host,
            Target::Guest => &mut self.guest,
        }
    }

    fn broadcast(&mut self, line: &str) {
        self.clients
            .retain(|_, s| s.write_all(format!("{line}\n").as_bytes()).is_ok());
    }

    fn tell_agent(&mut self, line: &str) {
        if let Some((_, s)) = &mut self.agent
            && s.write_all(format!("{line}\n").as_bytes()).is_err()
        {
            self.agent = None;
        }
    }

    fn switch(&mut self, to: Target, y: Option<f64>) {
        let from = self.router.target();
        if to == from {
            return;
        }
        if to == Target::Guest && !self.available {
            eprintln!("kvm-switch: nothing reads the guest devices, staying on the host");
            return;
        }
        let mut releases = self.router.switch(to);
        if from == Target::Guest {
            if let Some((_, tp)) = &mut self.touchpad {
                let held = tp.end();
                self.touch_out(held);
            }
            let held = self.pointer.release_all();
            releases.extend(held.into_iter().map(|e| (Output::Mouse, e)));
        }
        self.pair(from).emit(&releases);
        self.grab_touchpad(to == Target::Guest);
        eprintln!("kvm-switch: -> {}", to.name());
        self.broadcast(&format!("target {}", to.name()));
        match (to, y) {
            (Target::Guest, Some(y)) => self.tell_agent(&format!("enter {y:.4}")),
            (Target::Guest, None) => self.tell_agent("enter"),
            (Target::Host, _) => self.tell_agent("leave"),
        }
    }

    fn command(&mut self, line: &str) {
        let mut words = line.split_whitespace();
        let y = |w: Option<&str>| parse_fraction(w);
        match words.next() {
            Some("guest") => self.switch(Target::Guest, y(words.next())),
            Some("host") => self.switch(Target::Host, y(words.next())),
            Some("toggle") => match self.router.target() {
                Target::Host => self.switch(Target::Guest, None),
                Target::Guest => self.switch(Target::Host, None),
            },
            _ => eprintln!("kvm-switch: ignored {line:?}"),
        }
    }

    fn grab_touchpad(&mut self, grab: bool) {
        let Some((fd, tp)) = &mut self.touchpad else {
            return;
        };
        if grab {
            tp.begin();
        }
        if unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGRAB, libc::c_int::from(grab)) } < 0 {
            eprintln!(
                "kvm-switch: touchpad grab {grab}: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Writes touchpad output to the guest.
    fn touch_out(&mut self, outs: Vec<TouchOut>) {
        for out in outs {
            let events = match out {
                TouchOut::Motion(dx, dy) => self.pointer.motion(dx, dy),
                TouchOut::Scroll(v, h) => self.pointer.scroll(v, h),
                TouchOut::Button(code, down) => self.pointer.button(code, down),
                TouchOut::Keys(keys) => {
                    let key = |code, value| {
                        (
                            Output::Keyboard,
                            Ev {
                                kind: EV_KEY,
                                code,
                                value,
                            },
                        )
                    };
                    let press: Vec<_> = keys.iter().map(|&k| key(k, 1)).collect();
                    let release: Vec<_> = keys.iter().rev().map(|&k| key(k, 0)).collect();
                    self.guest.emit(&press);
                    self.guest.emit(&release);
                    continue;
                }
            };
            let events: Vec<_> = events.into_iter().map(|e| (Output::Mouse, e)).collect();
            self.guest.emit(&events);
        }
    }

    fn tick(&mut self) {
        if !self.watching_touchpad {
            self.watching_touchpad = true;
            let tx = self.tx.clone();
            thread::spawn(move || watch_touchpad(tx));
        }
        for node in candidates() {
            if self.known.insert(node.clone()) {
                let tx = self.tx.clone();
                thread::spawn(move || watch(node, tx));
            }
        }
        let available = consumed(&self.guest_nodes);
        if available != self.available {
            self.available = available;
            eprintln!(
                "kvm-switch: guest devices {}",
                if available { "in use" } else { "unused" }
            );
            self.broadcast(&format!("available {}", u8::from(available)));
            if !available {
                self.switch(Target::Host, None);
            }
        }
    }

    fn frame(&mut self, events: Vec<Ev>) {
        let mut out = Vec::new();
        let mut action = None;
        for ev in events {
            action = self.router.input(ev, &mut out).or(action);
        }
        let target = self.router.target();
        self.pair(target).emit(&out);
        match action {
            Some(Action::Toggle) => self.command("toggle"),
            Some(Action::Quit) => {
                let releases = self.router.release_all();
                self.pair(target).emit(&releases);
                eprintln!("kvm-switch: quit requested from the keyboard");
                std::process::exit(0);
            }
            None => {}
        }
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let (tx, rx) = mpsc::channel();
    let host = Pair::new("host").context("create host uinput devices")?;
    let mut guest = Pair::new("guest").context("create guest uinput devices")?;
    let guest_nodes = guest.nodes()?;
    listen_control(args.socket_gid, tx.clone())?;
    listen_vsock(args.vsock_port, args.guest_cid, tx.clone())?;
    {
        let tx = tx.clone();
        thread::spawn(move || {
            while tx.send(Msg::Tick).is_ok() {
                thread::sleep(Duration::from_millis(500));
            }
        });
    }
    let mut state = State {
        router: Router::new(),
        host,
        guest,
        guest_nodes,
        available: false,
        clients: HashMap::new(),
        agent: None,
        touchpad: None,
        watching_touchpad: false,
        pointer: GuestPointer::default(),
        known: HashSet::new(),
        tx,
    };
    notify_ready();
    for msg in rx {
        match msg {
            Msg::Frame(events) => state.frame(events),
            Msg::Touchpad(fd, tp) => {
                state.touchpad = Some((fd, tp));
                if state.router.target() == Target::Guest {
                    state.grab_touchpad(true);
                }
            }
            Msg::TouchFrame(events, ms) => {
                if let Some((_, tp)) = &mut state.touchpad {
                    let outs = tp.frame(&events, ms);
                    if state.router.target() == Target::Guest {
                        state.touch_out(outs);
                    }
                }
            }
            Msg::TouchpadGone => {
                state.touchpad = None;
                state.watching_touchpad = false;
            }
            Msg::DeviceGone(path) => {
                state.known.remove(Path::new(&path));
            }
            Msg::Client(id, mut stream) => {
                let hello = format!(
                    "available {}\ntarget {}\n",
                    u8::from(state.available),
                    state.router.target().name()
                );
                if stream.write_all(hello.as_bytes()).is_ok() {
                    state.clients.insert(id, stream);
                }
            }
            Msg::ClientLine(line) => state.command(&line),
            Msg::ClientGone(id) => {
                state.clients.remove(&id);
            }
            Msg::Agent(id, stream) => {
                eprintln!("kvm-switch: guest agent connected");
                state.agent = Some((id, stream));
                if state.router.target() == Target::Guest {
                    state.tell_agent("enter");
                }
            }
            Msg::AgentLine(line) => {
                let mut words = line.split_whitespace();
                if words.next() == Some("host") {
                    state.switch(Target::Host, parse_fraction(words.next()));
                }
            }
            Msg::AgentGone(id) => {
                if state.agent.as_ref().is_some_and(|(a, _)| *a == id) {
                    eprintln!("kvm-switch: guest agent disconnected");
                    state.agent = None;
                }
            }
            Msg::Tick => state.tick(),
        }
    }
    Ok(())
}
