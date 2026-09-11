{
  description = "merlin — a Matrix assistant";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    # Splits dependency compilation from crate compilation. buildRustPackage puts
    # both in one derivation, so editing one line rebuilt all four hundred
    # dependencies, which is the whole of the fourteen minute build.
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, crane, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs:
        # The sandbox is Linux-only: it rests on user namespaces and bubblewrap.
        # Darwin still builds the bot itself, which is what the dev shell needs.
        nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        inherit (self.legacyPackages.${pkgs.stdenv.hostPlatform.system}) sandbox-rootfs merlin-sandbox;
      } // {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.merlin;

        merlin =
          let
            craneLib = crane.mkLib pkgs;
            commonArgs = {
              src = craneLib.cleanCargoSource ./.;
              strictDeps = true;
              # aws-lc-sys arrives through rustls, which matrix-sdk selects with
              # no opt-out. It needs cmake and a C toolchain at build time.
              nativeBuildInputs = with pkgs; [ pkg-config cmake ];
              buildInputs = with pkgs; [ openssl ];
            };
          in
          craneLib.buildPackage (commonArgs // {
            pname = "merlin";
            version = "0.1.0";
            # Dependencies become their own derivation, keyed on Cargo.lock
            # rather than on the source, so a code change rebuilds this crate
            # alone and everything under it comes from the cache.
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;
            doCheck = false;
            meta = {
              description = "Matrix assistant with memory, tools and scheduled jobs";
              mainProgram = "merlin";
            };
          });

      });

      legacyPackages = forAll (pkgs: nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        # The base userland for the sandbox, unpacked once into a persistent
        # directory. Alpine rather than a Nix closure because the point is that
        # the agent can install its own tools, and apk is a package manager it
        # can drive on its own.
        sandbox-rootfs =
          let
            version = "3.24.1";
            source = {
              x86_64-linux = {
                arch = "x86_64";
                hash = "sha256-Qfc+PPX6kZuKpcprMNxI8NonIHdtdCPip3SCEUVv4IE=";
              };
              aarch64-linux = {
                arch = "aarch64";
                hash = "sha256-9VqQ9pBSxb1vkssJqPRwZZcIMLGUyRegBvuUAo5yElk=";
              };
            }.${pkgs.stdenv.hostPlatform.system} or null;
          in
          if source == null then null else
          pkgs.fetchurl {
            url = "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/${source.arch}/alpine-minirootfs-${version}-${source.arch}.tar.gz";
            inherit (source) hash;
          };

        # The only thing merlin may invoke through sudo. It takes a language on
        # argv and a script on stdin, and runs it inside a persistent root that
        # has no view of the host at all.
        merlin-sandbox = pkgs.writeShellApplication {
          name = "merlin-sandbox";
          runtimeInputs = with pkgs; [ bubblewrap coreutils ];
          text = ''
            set -uo pipefail

            # Taken from argv, not the environment: sudo runs with env_reset,
            # so anything exported by the caller is stripped before this runs.
            lang="''${1:-bash}"
            timeout_s="''${2:-60}"
            address_space_kb="''${3:-0}"
            root="''${MERLIN_SANDBOX_ROOT:-/var/lib/merlin-sandbox}"
            work="''${MERLIN_WORKSPACE:-/var/lib/merlin-workspace}"

            if [ ! -x "$root/bin/busybox" ]; then
              echo "sandbox root at $root is not initialised" >&2
              exit 3
            fi

            job="$(mktemp -d)"
            trap 'rm -rf "$job"' EXIT
            cat > "$job/script"

            case "$lang" in
              python) interp=(python3 /job/script) ;;
              bash|sh) interp=(/bin/sh /job/script) ;;
              *) echo "unsupported language: $lang" >&2; exit 2 ;;
            esac

            # The workspace is shared with the bot through a group, and the
            # default 022 would leave everything the sandbox writes read-only to
            # it, so edit_file would fail on the sandbox's own output.
            umask 007

            # A runaway process count is the one resource bwrap does not bound,
            # and a fork bomb inside the namespace is still host processes.
            ulimit -u 512 || true
            ulimit -f 4194304 || true
            if [ "$address_space_kb" -gt 0 ]; then
              ulimit -v "$address_space_kb" || true
            fi

            # --unshare-all drops every namespace, --share-net puts the network
            # back so the agent can fetch and install things. --unshare-user
            # with --uid 0 makes it root inside its own root only: apk works,
            # while the host sees an unprivileged uid that the firewall matches
            # on. Nothing from the host is bound in, so there is no path to
            # /nix/store, /var/lib/merlin or /run/secrets to begin with.
            exec timeout --signal=KILL "$timeout_s" \
              bwrap \
                --unshare-all --share-net \
                --unshare-user --uid 0 --gid 0 \
                --cap-drop ALL \
                --die-with-parent \
                --new-session \
                --bind "$root" / \
                --bind "$work" /work \
                --ro-bind "$job" /job \
                --proc /proc \
                --dev /dev \
                --tmpfs /run \
                --chdir /work \
                --setenv HOME /root \
                --setenv PATH /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
                --setenv TMPDIR /tmp \
                "''${interp[@]}"
          '';
        };
      });

      nixosModules.default = import ./nix/module.nix self;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rust-analyzer clippy pkg-config cmake openssl sqlite ];
        };
      });
    };
}
