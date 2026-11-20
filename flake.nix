{
  description = "DashBeam development shell (Rust + Tauri + React + WASM)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f system (
            import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            }
          )
        );

      # Single source of truth for the Rust version: rust-toolchain.toml.
      # rust-overlay wants a full x.y.z, the repo pins the shorter "1.91".
      rustChannel = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
      rustVersion =
        if builtins.match "[0-9]+\\.[0-9]+" rustChannel != null then "${rustChannel}.0" else rustChannel;

      # wasm-bindgen-cli must match the wasm-bindgen crate exactly or the
      # generated bindings fail at load time. Read the pin out of the lockfile
      # to pick the matching nixpkgs package, and warn when there isn't one.
      wasmBindgenPin =
        let
          matches = builtins.filter (p: p.name == "wasm-bindgen") (
            (builtins.fromTOML (builtins.readFile ./wasm-bridge/Cargo.lock)).package
          );
        in
        if matches == [ ] then null else (builtins.head matches).version;
    in
    {
      devShells = forAllSystems (
        system: pkgs:
        let
          inherit (pkgs) lib stdenv;

          # nixpkgs keeps versioned copies (wasm-bindgen-cli_0_2_126, ...);
          # use the one matching the lockfile, else the default.
          wasmBindgenCli =
            if wasmBindgenPin == null then
              pkgs.wasm-bindgen-cli
            else
              pkgs."wasm-bindgen-cli_${lib.replaceStrings [ "." ] [ "_" ] wasmBindgenPin}"
                or pkgs.wasm-bindgen-cli;

          mkRustToolchain =
            extraTargets:
            (
              if builtins.elem rustVersion [ "stable" "beta" "nightly" ] then
                pkgs.rust-bin.${rustVersion}.latest.default
              else
                pkgs.rust-bin.stable.${rustVersion}.default
            ).override
              {
                extensions = [
                  "rust-src"
                  "rust-analyzer"
                  "clippy"
                  "rustfmt"
                ];
                # web target: pnpm build:wasm / pnpm dev:web
                targets = [ "wasm32-unknown-unknown" ] ++ extraTargets;
              };

          iosTargets = [
            "aarch64-apple-ios" # devices
            "aarch64-apple-ios-sim" # simulator, Apple silicon
            "x86_64-apple-ios" # simulator, Intel
          ];

          # iOS builds must use Xcode's toolchain, but rust-overlay's toolchain
          # propagates nix's cc (clang + cctools, their wrappers and a
          # macOS-only apple-sdk), which forces macOS flags onto every
          # compile/link and hides the iphoneos/iphonesimulator SDKs.
          iosShellHook = ''
            PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vE -- '^/nix/store/[a-z0-9]+-(clang|cctools|bintools)-' | paste -sd: -)"
            unset LD DEVELOPER_DIR SDKROOT
            # Pin by absolute path: a `cc` elsewhere on PATH (e.g. a
            # user-profile gcc) would otherwise win and can't link against
            # Apple SDKs.
            export CC=/usr/bin/clang CXX=/usr/bin/clang++ AR=/usr/bin/ar
            for t in AARCH64_APPLE_DARWIN X86_64_APPLE_DARWIN AARCH64_APPLE_IOS AARCH64_APPLE_IOS_SIM X86_64_APPLE_IOS; do
              export "CARGO_TARGET_''${t}_LINKER=/usr/bin/clang"
            done
            # nix gives each shell a private TMPDIR, but `tauri ios dev` and
            # the Xcode build phase (src-tauri/ios/build-rust-code.sh, which
            # enters this shell itself) find each other through a file in
            # TMPDIR. Use the stable per-user temp dir so both shells agree.
            export TMPDIR="$(/usr/bin/getconf DARWIN_USER_TEMP_DIR)"
          '';

          # WebKitGTK + GTK stack that `tauri dev` links and dlopens on Linux.
          # Mirrors the apt list in .github/workflows/publish.yml.
          guiLibs = with pkgs; [
            webkitgtk_4_1
            gtk3
            glib
            libsoup_3
            cairo
            pango
            gdk-pixbuf
            at-spi2-atk
            harfbuzz
            librsvg
            libayatana-appindicator # tray-icon feature in src-tauri
            dbus # tray + notifications
            openssl
          ];

          # Icon themes and gsettings schemas the webview and file dialogs read.
          dataDirs = with pkgs; [
            "${gsettings-desktop-schemas}/share/gsettings-schemas/${gsettings-desktop-schemas.name}"
            "${gtk3}/share/gsettings-schemas/${gtk3.name}"
            "${adwaita-icon-theme}/share"
            "${hicolor-icon-theme}/share"
          ];

          # ring compiles C for wasm32 (pnpm build:wasm). Nix's clang wrapper
          # injects host target flags and Apple's clang (iOS shell) has no
          # wasm32 backend, so point the wasm32 target at the unwrapped tools;
          # cc-rs prefers these over CC.
          wasmEnv = {
            CC_wasm32_unknown_unknown = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
            AR_wasm32_unknown_unknown = "${pkgs.llvmPackages.bintools-unwrapped}/bin/llvm-ar";
          };

          linuxEnv = {
            # cargo-built binaries are not patchelf'd, so the GTK/WebKit stack
            # has to be resolvable at run time too, not just at link time.
            LD_LIBRARY_PATH = lib.makeLibraryPath guiLibs;

            # TLS in the webview (updater, relay status) needs glib-networking.
            GIO_MODULE_DIR = "${pkgs.glib-networking}/lib/gio/modules";

            # Blank/black window on NVIDIA and older Mesa. Harmless elsewhere;
            # drop it if your GPU renders the webview fine with DMA-BUF.
            WEBKIT_DISABLE_DMABUF_RENDERER = "1";
          };
          mkDevShell =
            {
              ios ? false,
            }:
            # mkShellNoCC for iOS: no nix cc from the stdenv (see iosShellHook).
            (if ios then pkgs.mkShellNoCC else pkgs.mkShell) (
              {
                name = if ios then "dashbeam-ios" else "dashbeam";

                nativeBuildInputs = [
                  (mkRustToolchain (lib.optionals ios iosTargets))
                  pkgs.nodejs_24
                  pkgs.pnpm
                  pkgs.pkg-config
                  wasmBindgenCli
                ]
                ++ lib.optionals ios [
                  pkgs.cocoapods
                  pkgs.xcodegen # tauri ios init
                ]
                ++ lib.optionals stdenv.isLinux (
                  with pkgs;
                  [
                    gobject-introspection
                    clang # ring builds its asm for wasm32 with clang
                    llvmPackages.bintools # llvm-ar for the wasm32 target
                    patchelf # tauri bundler
                    xdg-utils # tauri bundler
                  ]
                );

                buildInputs = lib.optionals stdenv.isLinux guiLibs;

                shellHook = ''
                  ${lib.optionalString ios iosShellHook}
                  ${lib.optionalString stdenv.isLinux ''
                    export XDG_DATA_DIRS="${lib.concatStringsSep ":" dataDirs}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
                  ''}
                  echo "DashBeam dev shell${lib.optionalString ios " (iOS)"}"
                  echo "  rustc   $(rustc --version | cut -d' ' -f2) (pinned ${rustVersion} in rust-toolchain.toml)"
                  echo "  node    $(node --version)"
                  echo "  pnpm    $(pnpm --version)"
                  ${lib.optionalString (wasmBindgenPin != null) ''
                    have_wb="$(wasm-bindgen --version 2>/dev/null | cut -d' ' -f2)"
                    if [ "$have_wb" != "${wasmBindgenPin}" ]; then
                      echo
                      echo "  warning: wasm-bindgen-cli is $have_wb, wasm-bridge/Cargo.lock pins ${wasmBindgenPin}."
                      echo "           The web target needs an exact match:"
                      echo "             cargo install wasm-bindgen-cli --version ${wasmBindgenPin} --locked"
                      echo "           (then make sure ~/.cargo/bin precedes the nix one on PATH)"
                    fi
                  ''}
                  echo
                  echo "  pnpm install && pnpm tauri dev    desktop"
                  echo "  pnpm build:wasm && pnpm dev:web   browser"
                  ${
                    if ios then
                      ''echo "  pnpm tauri ios dev --open          iOS, then Run in Xcode"''
                    else
                      lib.optionalString stdenv.isDarwin ''echo "  nix develop .#ios                  iOS shell"''
                  }
                '';
              }
              // wasmEnv
              // lib.optionalAttrs stdenv.isLinux linuxEnv
            );
        in
        {
          default = mkDevShell { };
        }
        # Keyed on the system name with nixpkgs.lib, not pkgs: listing the
        # shells must not import nixpkgs for the system, which throws on
        # platforms nixpkgs has dropped (x86_64-darwin).
        // nixpkgs.lib.optionalAttrs (nixpkgs.lib.hasSuffix "-darwin" system) {
          ios = mkDevShell { ios = true; };
        }
      );
    };
}
