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

      # Shared by the dependency build and the crate build, which must agree or
      # the artifacts are rejected and everything recompiles.
      craneArgs = pkgs: {
        src = (crane.mkLib pkgs).cleanCargoSource ./.;
        strictDeps = true;
        # aws-lc-sys arrives through rustls, which matrix-sdk selects with no
        # opt-out. It needs cmake and a C toolchain at build time.
        nativeBuildInputs = with pkgs; [ pkg-config cmake ];
        buildInputs = with pkgs; [ openssl ];
      };
    in
    {
      packages = forAll (pkgs:
        # The sandbox is Linux-only: it rests on user namespaces and bubblewrap.
        # Darwin still builds the bot itself, which is what the dev shell needs.
        nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        inherit (self.legacyPackages.${pkgs.stdenv.hostPlatform.system}) sandbox-rootfs merlin-sandbox;
      } // {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.merlin;

        # The compiled dependency graph, keyed on Cargo.lock rather than on the
        # source. Exposed as a package of its own because it is a build input
        # rather than a runtime one, so it is absent from merlin's closure and
        # `cachix push .#merlin` would never carry it. Caching this is the whole
        # point of splitting it out.
        merlin-deps = (crane.mkLib pkgs).buildDepsOnly (craneArgs pkgs);

        merlin = (crane.mkLib pkgs).buildPackage (craneArgs pkgs // {
          pname = "merlin";
          version = "0.1.0";
          cargoArtifacts = self.packages.${pkgs.stdenv.hostPlatform.system}.merlin-deps;
          doCheck = false;
          meta = {
            description = "Matrix assistant with memory, tools and scheduled jobs";
            mainProgram = "merlin";
          };
        });

      });

      legacyPackages = forAll (pkgs: nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        # The base userland for the sandbox, unpacked once and mounted read-only.
        # Alpine keeps the runtime compact while still providing ordinary tools.
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

        # The only thing merlin may invoke through sudo. It takes a script on
        # stdin and runs it inside a read-only root with no view of the host.
        merlin-sandbox = pkgs.writeShellApplication {
          name = "merlin-sandbox";
          runtimeInputs = with pkgs; [ bubblewrap coreutils ];
          text = ''
            set -uo pipefail

            # Taken from argv, not the environment: sudo runs with env_reset,
            # so anything exported by the caller is stripped before this runs.
            timeout_s="''${1:-60}"
            address_space_kb="''${2:-0}"
            scope="''${3:-0000000000000000000000000000000000000000000000000000000000000000}"
            root="''${MERLIN_SANDBOX_ROOT:-/var/lib/merlin-sandbox}"
            work_base="''${MERLIN_WORKSPACE:-/var/lib/merlin-workspace}"

            case "$scope" in
              *[!0-9a-f]*) echo "invalid sandbox scope" >&2; exit 2 ;;
            esac
            if [ "''${#scope}" -ne 64 ]; then
              echo "invalid sandbox scope" >&2
              exit 2
            fi
            umask 007
            work="$work_base/$scope"
            # Bind destinations must already exist because the base root is
            # mounted read-only before bwrap applies the room/job mounts.
            mkdir -p "$work" "$root/work" "$root/job"

            if [ ! -x "$root/bin/busybox" ]; then
              echo "sandbox root at $root is not initialised" >&2
              exit 3
            fi

            job="$(mktemp -d)"
            trap 'rm -rf "$job"' EXIT
            cat > "$job/script"

            # The workspace is shared with the bot through a group, and the
            # default 022 would leave everything the sandbox writes read-only to
            # it, so edit_file would fail on the sandbox's own output.
            # A runaway process count is the one resource bwrap does not bound,
            # and a fork bomb inside the namespace is still host processes.
            ulimit -u 512 || true
            ulimit -f 4194304 || true
            if [ "$address_space_kb" -gt 0 ]; then
              ulimit -v "$address_space_kb" || true
            fi

            # --unshare-all drops every namespace, --share-net puts the network
            # back so the agent can fetch public resources. --unshare-user with
            # --uid 0 makes it root inside its own read-only root, while the host
            # sees an unprivileged uid that the firewall matches
            # on. Nothing from the host is bound in, so there is no path to
            # /nix/store, /var/lib/merlin or /run/secrets to begin with. The
            # root is read-only and only this room's workspace is mounted, so
            # executed code cannot persist or read information across rooms.
            exec timeout --signal=KILL "$timeout_s" \
              bwrap \
                --unshare-all --share-net \
                --unshare-user --uid 0 --gid 0 \
                --cap-drop ALL \
                --die-with-parent \
                --new-session \
                --ro-bind "$root" / \
                --bind "$work" /work \
                --ro-bind "$job" /job \
                --proc /proc \
                --dev /dev \
                --tmpfs /run \
                --tmpfs /tmp \
                --chdir /work \
                --setenv HOME /root \
                --setenv PATH /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
                --setenv TMPDIR /tmp \
                /bin/sh /job/script
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
