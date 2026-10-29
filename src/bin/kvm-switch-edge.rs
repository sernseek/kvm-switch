//! A 1 px transparent layer-shell strip along the right edge of one output.
//! Pushing the pointer `threshold` px further right while it is on the strip
//! asks kvm-switch-router to hand the input to the guest. The strip exists
//! only while the router reports the guest devices in use, so it never
//! steals clicks otherwise.
//!
//!   kvm-switch-edge [--output eDP-1] [--threshold 50]

use anyhow::{Context, Result};
use kvm_switch::socket_path;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop,
            channel::{self, Event as ChannelEvent, Sender},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
        protocols::wp::{
            cursor_shape::v1::client::wp_cursor_shape_device_v1::{self, Shape},
            relative_pointer::zv1::client::zwp_relative_pointer_v1,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
        relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    thread,
    time::Duration,
};

enum RouterMsg {
    Connected(UnixStream),
    Line(String),
    Disconnected,
}

/// Keeps a connection to the router, reconnecting while it is not running.
fn connect_router(tx: Sender<RouterMsg>) {
    thread::spawn(move || {
        loop {
            if let Ok(stream) = UnixStream::connect(socket_path())
                && let Ok(writer) = stream.try_clone()
            {
                if tx.send(RouterMsg::Connected(writer)).is_err() {
                    return;
                }
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(RouterMsg::Line(line)).is_err() {
                        return;
                    }
                }
                if tx.send(RouterMsg::Disconnected).is_err() {
                    return;
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

struct Edge {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    relative_pointer_state: RelativePointerState,
    cursor_shape: Option<CursorShapeManager>,

    output_name: String,
    threshold: f64,

    pointer: Option<wl_pointer::WlPointer>,
    relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    shape_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    strip: Option<LayerSurface>,
    height: u32,

    router: Option<UnixStream>,
    available: bool,
    on_guest: bool,
    inside: bool,
    y: f64,
    push: f64,
    /// For the router source, whose callback gets no queue handle.
    qh: QueueHandle<Self>,
}

impl Edge {
    /// Creates or removes the strip to match the router's state.
    fn update_strip(&mut self, qh: &QueueHandle<Self>) {
        let want = self.router.is_some() && self.available;
        if !want {
            self.strip = None;
            self.inside = false;
            return;
        }
        if self.strip.is_some() {
            return;
        }
        let output = self.output_state.outputs().find(|o| {
            self.output_state
                .info(o)
                .is_some_and(|i| i.name.as_deref() == Some(self.output_name.as_str()))
        });
        let Some(output) = output else { return };
        let surface = self.compositor.create_surface(qh);
        let strip = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("kvm-switch-edge"),
            Some(&output),
        );
        strip.set_anchor(Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM);
        strip.set_size(1, 0);
        // Over panels and their exclusive zones too.
        strip.set_exclusive_zone(-1);
        strip.set_keyboard_interactivity(KeyboardInteractivity::None);
        strip.commit();
        self.strip = Some(strip);
    }

    fn send(&mut self, line: &str) {
        if let Some(router) = &mut self.router
            && router.write_all(format!("{line}\n").as_bytes()).is_err()
        {
            self.router = None;
        }
    }

    fn router_line(&mut self, line: &str, qh: &QueueHandle<Self>) {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("available"), Some(v)) => {
                self.available = v == "1";
                self.update_strip(qh);
            }
            (Some("target"), Some(t)) => {
                self.on_guest = t == "guest";
                self.push = 0.0;
            }
            _ => {}
        }
    }

    fn request_guest(&mut self) {
        let y = if self.height > 0 {
            (self.y / f64::from(self.height)).clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.send(&format!("guest {y:.4}"));
    }

    fn is_strip(&self, surface: &wl_surface::WlSurface) -> bool {
        self.strip
            .as_ref()
            .is_some_and(|s| s.wl_surface() == surface)
    }
}

impl CompositorHandler for Edge {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Edge {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.update_strip(qh);
    }
    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.update_strip(qh);
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Edge {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.strip = None;
        self.inside = false;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let width = configure.new_size.0.max(1);
        let height = configure.new_size.1.max(1);
        self.height = height;
        // Fully transparent; the input region still covers the whole strip.
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) else {
            eprintln!("kvm-switch-edge: cannot allocate the strip buffer");
            return;
        };
        canvas.fill(0);
        layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        if buffer.attach_to(layer.wl_surface()).is_ok() {
            layer.commit();
        }
    }
}

impl SeatHandler for Edge {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer || self.pointer.is_some() {
            return;
        }
        let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) else {
            return;
        };
        self.relative_pointer = self
            .relative_pointer_state
            .get_relative_pointer(&pointer, qh)
            .ok();
        self.shape_device = self
            .cursor_shape
            .as_ref()
            .map(|m| m.get_shape_device(&pointer, qh));
        self.pointer = Some(pointer);
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(r) = self.relative_pointer.take() {
                r.destroy();
            }
            if let Some(d) = self.shape_device.take() {
                d.destroy();
            }
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Edge {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if !self.is_strip(&event.surface) {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.inside = true;
                    self.push = 0.0;
                    self.y = event.position.1;
                    if let Some(d) = &self.shape_device {
                        d.set_shape(serial, Shape::Default);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.inside = false;
                    self.push = 0.0;
                }
                PointerEventKind::Motion { .. } => self.y = event.position.1,
                _ => {}
            }
        }
    }
}

impl RelativePointerHandler for Edge {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        if !self.inside || self.on_guest {
            return;
        }
        let dx = event.delta.0;
        if dx > 0.0 {
            self.push += dx;
        } else if dx < 0.0 {
            self.push = 0.0;
        }
        if self.push >= self.threshold {
            self.push = 0.0;
            self.request_guest();
        }
    }
}

impl ShmHandler for Edge {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Edge);

impl ProvidesRegistryState for Edge {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(Edge);

fn main() -> Result<()> {
    let mut output_name = String::from("eDP-1");
    let mut threshold = 50.0;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--output" => output_name = value,
            "--threshold" => threshold = value.parse()?,
            _ => anyhow::bail!("unknown argument {flag}"),
        }
    }

    let conn = Connection::connect_to_env().context("connect to the Wayland compositor")?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<Edge> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert the Wayland source: {e}"))?;

    let (tx, rx) = channel::channel();
    connect_router(tx);
    event_loop
        .handle()
        .insert_source(rx, |event, _, edge: &mut Edge| {
            let ChannelEvent::Msg(msg) = event else {
                return;
            };
            let qh = edge.qh.clone();
            match msg {
                RouterMsg::Connected(stream) => edge.router = Some(stream),
                RouterMsg::Line(line) => edge.router_line(&line, &qh),
                RouterMsg::Disconnected => {
                    edge.router = None;
                    edge.available = false;
                    edge.on_guest = false;
                }
            }
            edge.update_strip(&qh);
        })
        .map_err(|e| anyhow::anyhow!("insert the router source: {e}"))?;

    let shm = Shm::bind(&globals, &qh).context("wl_shm")?;
    let mut edge = Edge {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
        pool: SlotPool::new(4096, &shm).context("shm pool")?,
        shm,
        relative_pointer_state: RelativePointerState::bind(&globals, &qh),
        cursor_shape: CursorShapeManager::bind(&globals, &qh).ok(),
        output_name,
        threshold,
        pointer: None,
        relative_pointer: None,
        shape_device: None,
        strip: None,
        height: 0,
        router: None,
        available: false,
        on_guest: false,
        inside: false,
        y: 0.0,
        push: 0.0,
        qh: qh.clone(),
    };
    loop {
        event_loop.dispatch(None, &mut edge)?;
    }
}
