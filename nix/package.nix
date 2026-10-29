# kvm-switch: one keyboard and mouse for the host and a passthrough guest on
# its own monitor (see src/lib.rs). The router and the edge strip are host
# programs; `guest` is the Windows agent, cross-built with mingw.
{
  lib,
  rustPlatform,
  pkgsCross,
  source ? ../.,
}:
let
  sourceDirectory = toString source;
  # Rust sources and manifests of the host crate and of guest/.
  sources =
    directory:
    builtins.path {
      name = "kvm-switch-source";
      path = "${sourceDirectory}${directory}";
      filter =
        path: type:
        let
          relative = lib.removePrefix "${sourceDirectory}${directory}" path;
        in
        (type == "directory" && (relative == "/src" || lib.hasPrefix "/src/" relative))
        || (
          type == "regular"
          && (
            builtins.elem relative [
              "/Cargo.toml"
              "/Cargo.lock"
            ]
            || (lib.hasPrefix "/src/" relative && lib.hasSuffix ".rs" relative)
          )
        );
    };
  src = sources "";
  guestSrc = sources "/guest";
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).package.version;
in
rustPlatform.buildRustPackage {
  pname = "kvm-switch";
  inherit src version;
  cargoLock.lockFile = "${src}/Cargo.lock";

  passthru.guest = pkgsCross.mingwW64.rustPlatform.buildRustPackage {
    pname = "kvm-switch-guest";
    inherit version;
    src = guestSrc;
    cargoLock.lockFile = "${guestSrc}/Cargo.lock";
    meta = {
      description = "Windows side of kvm-switch: hands the input back at the left edge";
      license = lib.licenses.mit;
      mainProgram = "kvm-switch-guest";
      platforms = lib.platforms.windows;
    };
  };

  meta = {
    description = "Share one keyboard and mouse between a Wayland host and a passthrough guest by screen edge";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
