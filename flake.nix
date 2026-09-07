# @file flake.nix
# @brief NixOS 开发环境与软件包
# @author modolet <y@xxyx.io>
# @date 2026-09-07
{
  description = "Linux Computer Use MCP for niri";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      dependencies =
        pkgs: with pkgs; [
          gtk4
          glib
          libxkbcommon
          wayland
          pipewire
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
        ];
      runtime =
        pkgs: with pkgs; [
          sway
          dbus
          firefox
          gnome-text-editor
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good
          pipewire
        ];
    in
    {
      packages = eachSystem (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "computer-use-linux";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = self;
              filter =
                path: type:
                pkgs.lib.cleanSourceFilter path type
                && !(builtins.elem (baseNameOf path) [
                  "target"
                  ".local-test"
                  ".direnv"
                  ".agents"
                  ".codex"
                ]);
            };
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgs; [
              pkg-config
              wrapGAppsHook4
            ];
            buildInputs = dependencies pkgs;
            preFixup = ''
              gappsWrapperArgs+=(--prefix PATH : "${pkgs.lib.makeBinPath (runtime pkgs)}")
              gappsWrapperArgs+=(--prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (runtime pkgs)}")
            '';
            meta.mainProgram = "computer-use-linux";
          };
        }
      );
      devShells = eachSystem (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              pkg-config
              nixfmt
              wl-clipboard
              niri
              shfmt
              shellcheck
            ];
            buildInputs = dependencies pkgs ++ runtime pkgs;
            GST_PLUGIN_SYSTEM_PATH_1_0 = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (runtime pkgs);
            TEST_ATSPI_LAUNCHER = "${pkgs.at-spi2-core}/libexec/at-spi-bus-launcher";
            TEST_ATSPI_REGISTRY = "${pkgs.at-spi2-core}/libexec/at-spi2-registryd";
          };
        }
      );
      homeManagerModules.default = import ./nix/home-manager.nix self;
    };
}
