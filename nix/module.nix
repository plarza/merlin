self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.merlin;
  inherit (lib) mkOption mkEnableOption mkIf types;

  system = pkgs.stdenv.hostPlatform.system;
  merlinPkg = self.packages.${system}.merlin;
  sandboxPkg = self.packages.${system}.merlin-sandbox;

  configFile = (pkgs.formats.toml { }).generate "merlin-config.toml" cfg.settings;
in
{
  options.services.merlin = {
    enable = mkEnableOption "the merlin Matrix assistant";

    package = mkOption {
      type = types.package;
      default = merlinPkg;
    };

    stateDir = mkOption {
      type = types.path;
      default = "/var/lib/merlin";
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Systemd EnvironmentFile carrying every credential: MATRIX_PASSWORD,
        OPENROUTER_API_KEY, EXA_API_KEY and optionally
        MATRIX_RECOVERY_PASSPHRASE. Nothing secret belongs in {option}`settings`,
        which is rendered world-readable into the Nix store.
      '';
    };

    soul = mkOption {
      type = types.lines;
      default = "";
      description = "Persona injected into the system prompt on every turn.";
    };

    settings = mkOption {
      type = types.attrs;
      default = { };
      description = "Rendered to config.toml.";
    };
  };

  config = mkIf cfg.enable {
    users.users.merlin = {
      isSystemUser = true;
      group = "merlin";
      home = cfg.stateDir;
    };
    users.groups.merlin = { };

    # Owns nothing. run_code executes as this user so that executed code cannot
    # read merlin's state or its EnvironmentFile, and so nftables has a uid to
    # match on for LAN egress.
    users.users.merlin-exec = {
      isSystemUser = true;
      group = "merlin-exec";
      home = "/var/empty";
    };
    users.groups.merlin-exec = { };

    # merlin may become merlin-exec, and only to run the sandbox wrapper. This
    # is not a path to root: the target user is less privileged than merlin.
    security.sudo.extraRules = [
      {
        users = [ "merlin" ];
        runAs = "merlin-exec";
        commands = [
          {
            command = "${lib.getExe sandboxPkg}";
            options = [ "NOPASSWD" "NOSETENV" ];
          }
        ];
      }
    ];

    # Executed code reaches the public internet but not the LAN. Without this
    # the sandbox would still see k3s, libsql, Grafana and the local registry.
    networking.nftables.enable = lib.mkDefault true;
    networking.nftables.ruleset = lib.mkAfter ''
      table inet merlin_exec {
        chain output {
          type filter hook output priority 0; policy accept;
          meta skuid "merlin-exec" ip daddr {
            10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16,
            169.254.0.0/16, 127.0.0.0/8
          } counter reject
          meta skuid "merlin-exec" ip6 daddr { ::1/128, fc00::/7, fe80::/10 } counter reject
        }
      }
    '';

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0750 merlin merlin -"
      "L+ ${cfg.stateDir}/SOUL.md - - - - ${pkgs.writeText "merlin-soul" cfg.soul}"
      "L+ ${cfg.stateDir}/config.toml - - - - ${configFile}"
    ];

    systemd.services.merlin = {
      description = "merlin Matrix assistant";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --config ${cfg.stateDir}/config.toml";
        User = "merlin";
        Group = "merlin";
        StateDirectory = "merlin";
        WorkingDirectory = cfg.stateDir;
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        Restart = "on-failure";
        RestartSec = "10s";

        # sudo needs to be reachable for run_code; everything else is closed.
        NoNewPrivileges = false;
        ProtectHome = true;
        PrivateTmp = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
      };

      environment.RUST_LOG = lib.mkDefault "merlin=info,warn";
    };
  };
}
