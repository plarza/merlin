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
    # read merlin's state or its EnvironmentFile, and so the firewall has a uid
    # to match on for LAN egress.
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
    #
    # Deliberately expressed as iptables rules inside the existing firewall
    # rather than by enabling networking.nftables: this host runs k3s, whose
    # kube-proxy programs iptables directly, and switching the backend
    # underneath it risks cluster networking.
    networking.firewall.extraCommands = ''
      iptables -w -C OUTPUT -m owner --uid-owner merlin-exec -d 10.0.0.0/8 -j REJECT 2>/dev/null || \
        iptables -w -I OUTPUT -m owner --uid-owner merlin-exec -d 10.0.0.0/8 -j REJECT
      iptables -w -C OUTPUT -m owner --uid-owner merlin-exec -d 172.16.0.0/12 -j REJECT 2>/dev/null || \
        iptables -w -I OUTPUT -m owner --uid-owner merlin-exec -d 172.16.0.0/12 -j REJECT
      iptables -w -C OUTPUT -m owner --uid-owner merlin-exec -d 192.168.0.0/16 -j REJECT 2>/dev/null || \
        iptables -w -I OUTPUT -m owner --uid-owner merlin-exec -d 192.168.0.0/16 -j REJECT
      iptables -w -C OUTPUT -m owner --uid-owner merlin-exec -d 169.254.0.0/16 -j REJECT 2>/dev/null || \
        iptables -w -I OUTPUT -m owner --uid-owner merlin-exec -d 169.254.0.0/16 -j REJECT
      iptables -w -C OUTPUT -m owner --uid-owner merlin-exec -d 127.0.0.0/8 -j REJECT 2>/dev/null || \
        iptables -w -I OUTPUT -m owner --uid-owner merlin-exec -d 127.0.0.0/8 -j REJECT
    '';

    networking.firewall.extraStopCommands = ''
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 10.0.0.0/8 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 172.16.0.0/12 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 192.168.0.0/16 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 169.254.0.0/16 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 127.0.0.0/8 -j REJECT 2>/dev/null || true
    '';

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0700 merlin merlin -"
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
        # StateDirectory defaults to 0755 and overrides the tmpfiles mode, which
        # left the message archive and memory readable by every user on the
        # host. UMask covers files the process creates afterwards.
        StateDirectoryMode = "0700";
        UMask = "0077";
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
