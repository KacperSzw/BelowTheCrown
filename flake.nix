{
  description = "Below the Crown: native LAN terminal castle";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay";
  inputs.rust-overlay.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      packagesFor = system: import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
      capFor = pkgs: pkgs.pkgsStatic.libcap.override { withGo = false; };
      helperFor = pkgs: pkgs.pkgsStatic.stdenv.mkDerivation {
        pname = "bubblewrap";
        version = "0.13.0";
        src = pkgs.fetchurl {
          url = "https://github.com/containers/bubblewrap/releases/download/v0.13.0/bubblewrap-0.13.0.tar.xz";
          hash = "sha256-RzQjdHPA5daV5OkDSjTkOy2/UWRlW9E/pZrjdrK3p2U=";
        };
        outputs = [ "out" ];
        nativeBuildInputs = [ pkgs.meson pkgs.ninja pkgs.pkg-config ];
        buildInputs = [ (capFor pkgs) ];
        mesonFlags = [ "-Dman=disabled" "-Dtests=false" "-Dselinux=disabled" "-Dbash_completion=disabled" "-Dzsh_completion=disabled" ];
        doCheck = false;
        meta = { description = "Pinned static worker sandbox helper"; license = pkgs.lib.licenses.lgpl2Plus; platforms = systems; mainProgram = "bwrap"; };
      };
      shellFor = pkgs: crossPkgs: target:
        let
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          compiler = crossPkgs.pkgsStatic.stdenv.cc;
          targetKey = builtins.replaceStrings [ "-" ] [ "_" ] target;
          linkerKey = "CARGO_TARGET_${pkgs.lib.toUpper targetKey}_LINKER";
          hostKey = builtins.replaceStrings [ "-" ] [ "_" ] pkgs.stdenv.hostPlatform.config;
          hostLinkerKey = "CARGO_TARGET_${pkgs.lib.toUpper hostKey}_LINKER";
          hostCompiler = "${pkgs.stdenv.cc}/bin/cc";
        in pkgs.mkShell ({
          packages = [ rust pkgs.pkg-config pkgs.git pkgs.chafa pkgs.imagemagick pkgs.binutils pkgs.file pkgs.jq pkgs.openssh pkgs.bubblewrap compiler ];
          BTC_SANDBOX_HELPER = "${helperFor crossPkgs}/bin/bwrap";
          HOST_CC = hostCompiler;
          shellHook = ''
            echo "Below the Crown · cargo test · cargo run -- host" >&2
            echo "Codex must already be installed and logged in." >&2
          '';
        } // {
          ${hostLinkerKey} = hostCompiler;
          "CC_${hostKey}" = hostCompiler;
          ${linkerKey} = "${compiler}/bin/${compiler.targetPrefix}cc";
          "CC_${targetKey}" = "${compiler}/bin/${compiler.targetPrefix}cc";
        });
    in {
      packages = eachSystem (system:
        let pkgs = packagesFor system; helper = helperFor pkgs;
        in {
          sandbox-helper = helper;
          sandbox-helper-aarch64 = helperFor pkgs.pkgsCross.aarch64-multiplatform;
          helper-sources = pkgs.runCommand "below-the-crown-helper-corresponding-source" {} ''
            mkdir -p "$out/docs"
            cp ${helper.src} "$out/bubblewrap-0.13.0.tar.xz"
            cp ${(capFor pkgs).src} "$out/libcap-${(capFor pkgs).version}.tar.xz"
            cp ${./flake.nix} "$out/flake.nix"
            cp ${./flake.lock} "$out/flake.lock"
            cp ${./rust-toolchain.toml} "$out/rust-toolchain.toml"
            cp ${./docs/sandbox.md} "$out/docs/sandbox.md"
            cp ${./NOTICES.md} "$out/NOTICES.md"
          '';
        });
      devShells = eachSystem (system:
        let
          pkgs = packagesFor system;
          target = if system == "x86_64-linux" then "x86_64-unknown-linux-musl" else "aarch64-unknown-linux-musl";
        in {
          default = shellFor pkgs pkgs target;
          cross-aarch64 = shellFor pkgs pkgs.pkgsCross.aarch64-multiplatform "aarch64-unknown-linux-musl";
        });
    };
}
