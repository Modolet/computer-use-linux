# @file home-manager.nix
# @brief 可选用户服务与 niri 停止快捷键
# @author modolet <y@xxyx.io>
# @date 2026-09-07
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.computer-use-linux;
in
{
  options.services.computer-use-linux = {
    enable = lib.mkEnableOption "Linux Computer Use MCP";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    };
  };
  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    systemd.user.services.computer-use-linux = {
      Unit = {
        Description = "Linux Computer Use MCP permission broker";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        UMask = "0077";
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
    xdg.configFile."computer-use-linux/niri.kdl".text = ''
      binds {
        Mod+Shift+Escape { spawn "${lib.getExe cfg.package}" "pause-all"; }
      }
    '';
  };
}
