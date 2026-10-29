# kvm-switch

One physical keyboard and mouse for a Linux host and a passthrough Windows
guest whose screen is a separate monitor, switched by screen edge like a
second display: push the pointer past the right edge of a host output and the
input moves to Windows; push past Windows' left edge and it comes back.

Unlike software KVMs that forward input over the network, the guest receives
its input as virtual hardware (uinput devices that QEMU reads), so the logon
screen, UAC prompts and games work as with a real keyboard and mouse.

## How it works

- **`kvm-switch-router`** (root) grabs the physical keyboards and mice and
  replays their events on one of two pairs of uinput devices: one the host
  compositor reads, one QEMU reads (libvirt `<input type='evdev'>`). It
  decides where the input goes and tells the other parts on a control socket.
- **`kvm-switch-edge`** (user, Wayland) is a 1 px transparent layer-shell
  strip along the right edge of one host output. Pushing `--threshold` px
  further right (measured with relative-pointer) asks the router to switch.
  The strip exists only while QEMU holds the guest devices, so it never steals
  clicks otherwise.
- **`kvm-switch-guest.exe`** (Windows, user session) watches raw mouse input
  and reports pushing `threshold` mickeys past the left edge of the desktop
  over vsock. When the input arrives, it places the pointer at the height it
  left the host.
- Both Alt keys together toggle the input; that is the only way on Windows'
  secure desktop. Both Alts + Backspace exits the router, which ungrabs
  everything. Held keys are released on the side they were pressed on.

The touchpad is read all the time but grabbed only while the guest has the
input, since QEMU cannot present a Windows precision touchpad: the router does
what libinput would. One finger moves (1000 dpi equivalent), two scroll
naturally, taps click (one finger left, two right, three middle), pressing
clicks by finger count, and three- and four-finger swipes send Windows'
shortcuts for the same gestures (Win+Tab, Win+D, Alt+Tab, Ctrl+Win+arrow).

The clipboard is not part of this project:
[clipboard-bridge](https://github.com/sernseek/clipboard-bridge) follows the
router's switches (`--guest-port`) and carries text, HTML, images and files.

## Protocols

Control socket (`/run/kvm-switch/control.sock`, `KVM_SWITCH_SOCKET` overrides
it; line based):

```text
-> available 0|1        whether something (QEMU) reads the guest devices
-> target host|guest
<- guest [Y], host, toggle      (Y: vertical position, 0..1)
```

vsock, from the guest agent (default port 8788, only `--guest-cid` accepted):

```text
-> enter [Y], leave
<- host [Y]
```

## Requirements

- Linux with uinput and evdev access for the router (root), and a Wayland
  compositor with `wlr-layer-shell`, `relative-pointer` and `cursor-shape` for
  the edge strip. Developed with niri.
- QEMU/libvirt reading the guest pair; the router already grabs the physical
  devices, so QEMU needs no grab:

  ```xml
  <input type='evdev'><source dev='/dev/input/kvm-switch-guest-kbd'/></input>
  <input type='evdev'><source dev='/dev/input/kvm-switch-guest-mouse'/></input>
  ```

  with udev rules that link the devices and hide them from libinput:

  ```text
  SUBSYSTEM=="input", KERNEL=="event*", ATTRS{name}=="kvm-switch guest keyboard", ENV{LIBINPUT_IGNORE_DEVICE}="1", SYMLINK+="input/kvm-switch-guest-kbd"
  SUBSYSTEM=="input", KERNEL=="event*", ATTRS{name}=="kvm-switch guest mouse", ENV{LIBINPUT_IGNORE_DEVICE}="1", SYMLINK+="input/kvm-switch-guest-mouse"
  ```

- A vsock device on the guest and the viosock driver (virtio-win), which
  registers AF_VSOCK as a Winsock provider.

## Usage

```console
kvm-switch-router [--vsock-port 8788] [--guest-cid 3] [--socket-gid GID]
kvm-switch-edge [--output eDP-1] [--threshold 50]
kvm-switch-guest.exe [port [threshold]]      (defaults 8788, 50)
```

Run the router as a system service while the guest runs, the edge strip as a
user service in the graphical session (`--socket-gid` lets that user connect),
and the guest agent as a Windows logon task in the user's session.

## Nix

This is a source input consumed with `callPackage`; it does not pin another
nixpkgs or require its own flake.

```nix
# flake.nix inputs
kvm-switch-src = {
  url = "github:sernseek/kvm-switch";
  flake = false;
};
```

```nix
kvmSwitch = pkgs.callPackage "${inputs.kvm-switch-src}/nix/package.nix" {
  source = inputs.kvm-switch-src;
};
# kvmSwitch.guest: the Windows agent, cross-built with mingw
```

## Development

```console
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

The routing state machine (`src/lib.rs`) and the touchpad's
(`src/touchpad.rs`) are free of I/O and have unit tests.

## License

MIT
