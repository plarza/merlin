self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.merlin;
  inherit (lib) mkOption mkEnableOption mkIf types;

  system = pkgs.stdenv.hostPlatform.system;
  merlinPkg = self.packages.${system}.merlin;
  sandboxPkg = self.packages.${system}.merlin-sandbox;
  rootfsTarball = self.packages.${system}.sandbox-rootfs;

  # Public resolvers, because the host's nameserver is a LAN address and LAN
  # egress is exactly what the sandbox is denied. Without this, name resolution
  # inside the sandbox fails and every fetch looks like a network outage.
  sandboxResolvConf = pkgs.writeText "merlin-sandbox-resolv.conf" ''
    nameserver 1.1.1.1
    nameserver 8.8.8.8
    options edns0
  '';

  configFile = (pkgs.formats.toml { }).generate "merlin-config.toml" cfg.settings;

  sandboxWrapper = pkgs.writeShellApplication {
    name = "merlin-sandbox-configured";
    text = ''
      export MERLIN_SANDBOX_ROOT=${cfg.sandboxRoot}/base
      export MERLIN_WORKSPACE=${cfg.workspaceDir}
      exec ${lib.getExe sandboxPkg} "$@"
    '';
  };
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

    sandboxRoot = mkOption {
      type = types.path;
      default = "/var/lib/merlin-sandbox";
      description = ''
        Base root filesystem for executed code. It is mounted read-only so
        durable data can only live in the room-scoped workspace.
      '';
    };

    workspaceDir = mkOption {
      type = types.path;
      default = "/var/lib/merlin-workspace";
      description = ''
        The agent's files, shared between the bot's file tools and the sandbox,
        where it appears as /work. Separate from {option}`sandboxRoot` so the
        bot never needs read access to the sandbox's operating system.
      '';
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Systemd EnvironmentFile carrying every credential: MATRIX_PASSWORD,
        OPENROUTER_API_KEY and EXA_API_KEY. Nothing secret belongs in
        {option}`settings`, which is rendered world-readable into the Nix store.
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
      extraGroups = [ "merlin-work" ];
      home = cfg.stateDir;
    };
    users.groups.merlin = { };

    # Owns nothing. run_code executes as this user so that executed code cannot
    # read merlin's state or its EnvironmentFile, and so the firewall has a uid
    # to match on for LAN egress.
    users.users.merlin-exec = {
      isSystemUser = true;
      group = "merlin-exec";
      extraGroups = [ "merlin-work" ];
      home = "/var/empty";
    };
    users.groups.merlin-exec = { };

    # The one thing the two users share: the workspace. The sandbox root itself
    # stays readable only by merlin-exec.
    users.groups.merlin-work = { };

    # merlin may become merlin-exec, and only to run the sandbox wrapper. This
    # is not a path to root: the target user is less privileged than merlin.
    security.sudo.extraRules = [
      {
        users = [ "merlin" ];
        runAs = "merlin-exec";
        commands = [
          {
            command = "${lib.getExe sandboxWrapper}";
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
      ${lib.concatMapStrings (net: ''
        ip6tables -w -C OUTPUT -m owner --uid-owner merlin-exec -d ${net} -j REJECT 2>/dev/null || \
          ip6tables -w -I OUTPUT -m owner --uid-owner merlin-exec -d ${net} -j REJECT
      '') [ "::1/128" "fe80::/10" "fc00::/7" ]}
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
      ${lib.concatMapStrings (net: ''
        ip6tables -w -D OUTPUT -m owner --uid-owner merlin-exec -d ${net} -j REJECT 2>/dev/null || true
      '') [ "::1/128" "fe80::/10" "fc00::/7" ]}
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 10.0.0.0/8 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 172.16.0.0/12 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 192.168.0.0/16 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 169.254.0.0/16 -j REJECT 2>/dev/null || true
      iptables -w -D OUTPUT -m owner --uid-owner merlin-exec -d 127.0.0.0/8 -j REJECT 2>/dev/null || true
    '';

    environment.systemPackages = [ sandboxWrapper ];

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0700 merlin merlin -"
      # Private to the sandbox uid: this is its operating system, not shared state.
      "d ${cfg.sandboxRoot} 0700 merlin-exec merlin-exec -"
      # Setgid so files created by either side stay group-writable by the other.
      "d ${cfg.workspaceDir} 2770 merlin merlin-work -"
      "L+ ${cfg.stateDir}/SOUL.md - - - - ${pkgs.writeText "merlin-soul" cfg.soul}"
      "L+ ${cfg.stateDir}/config.toml - - - - ${configFile}"
    ];

    # Unpacks a clean base userland once. It is mounted read-only at runtime;
    # the old mutable root is deliberately outside this base and never exposed.
    systemd.services.merlin-sandbox-init = {
      description = "Initialise merlin's sandbox root";
      wantedBy = [ "multi-user.target" ];
      before = [ "merlin.service" ];
      path = with pkgs; [ gnutar gzip coreutils ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        set -eu
        root="${cfg.sandboxRoot}/base"
        mkdir -p "$root"
        if [ ! -x "$root/bin/busybox" ]; then
          echo "unpacking base userland into $root"
          tar -xzf ${rootfsTarball} -C "$root"
          chown -R merlin-exec:merlin-exec "$root"
        fi

        # The tarball carries its own mode for ".", which overwrites the tmpfiles
        # rule and leaves the root world-readable. Reassert it after extracting.
        chmod 0700 "$root"
        install -m 0644 ${sandboxResolvConf} "$root/etc/resolv.conf"
        chown merlin-exec:merlin-exec "$root/etc/resolv.conf"
        mkdir -p "$root/work"

        # Install the fixed toolset before the root becomes read-only at runtime.
        if [ ! -x "$root/usr/bin/curl" ]; then
          echo "installing base tools"
          chroot "$root" /sbin/apk add --no-cache \
            bash curl git jq python3 py3-pip ripgrep file tar || \
            echo "base tool install failed" >&2
          chown -R merlin-exec:merlin-exec "$root"
        fi
      '';
    };

    systemd.services.merlin = {
      description = "merlin Matrix assistant";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" "merlin-sandbox-init.service" ];
      wants = [ "network-online.target" ];
      requires = [ "merlin-sandbox-init.service" ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --config ${cfg.stateDir}/config.toml";
        User = "merlin";
        Group = "merlin";
        StateDirectory = "merlin";
        # StateDirectory defaults to 0755 and overrides the tmpfiles mode, which
        # left the message archive and memory readable by every user on the
        # host. UMask covers files the process creates afterwards.
        StateDirectoryMode = "0700";
        # 0007 rather than 0077: the workspace is shared with the sandbox uid
        # through the merlin-work group, and 0077 would make every file the bot
        # writes unreadable to the code it then asks to run. The state directory
        # stays 0700, so this widens nothing outside the workspace.
        UMask = "0007";
        WorkingDirectory = cfg.stateDir;
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        Restart = "on-failure";
        RestartSec = "10s";

        # sudo needs to be reachable for run_code; everything else is closed.
        NoNewPrivileges = false;
        ProtectHome = true;
        PrivateTmp = true;
        # This remaps /proc/sys inside the service mount namespace, which makes
        # a descendant bubblewrap unable to mount the fresh /proc required by
        # its PID namespace. Both service users are unprivileged, so leaving
        # the host view in place does not let either of them change sysctls.
        ProtectKernelTunables = false;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        SupplementaryGroups = [ "merlin-work" ];
      };

      environment.RUST_LOG = lib.mkDefault "merlin=info,warn";

      # Spelled out rather than left to merlin's default of bare `sudo`, which
      # cannot work here for two reasons: a systemd unit's PATH holds neither
      # /run/wrappers/bin nor any other sudo, and the sudoers rule above names
      # the wrapper by store path, which the /run/current-system symlink does
      # not match. Deriving both sides from sandboxWrapper keeps them in step.
      environment.MERLIN_EXEC_RUNNER =
        "${config.security.wrapperDir}/sudo -n -u merlin-exec ${lib.getExe sandboxWrapper}";
    };
  };
}
