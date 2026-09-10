{
  description = "merlin — a Matrix assistant";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs = { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.merlin;

        merlin = pkgs.rustPlatform.buildRustPackage {
          pname = "merlin";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # aws-lc-sys arrives through rustls, which matrix-sdk selects with no
          # opt-out. It needs cmake and a C toolchain at build time.
          nativeBuildInputs = with pkgs; [ pkg-config cmake ];
          buildInputs = with pkgs; [ openssl ];

          meta = {
            description = "Matrix assistant with memory, tools and scheduled jobs";
            mainProgram = "merlin";
          };
        };

        # The only thing merlin may invoke through sudo. Kept minimal and
        # auditable: it takes a language on argv, source on stdin, and runs it
        # under a namespace with no view of the host filesystem.
        merlin-sandbox = pkgs.writeShellApplication {
          name = "merlin-sandbox";
          runtimeInputs = with pkgs; [ bubblewrap coreutils python3 bash ];
          text = ''
            set -uo pipefail

            lang="''${1:-python}"
            timeout_s="''${MERLIN_EXEC_TIMEOUT:-60}"

            work="$(mktemp -d)"
            trap 'rm -rf "$work"' EXIT
            cat > "$work/job"

            case "$lang" in
              python) interp=(python3 /work/job) ;;
              bash)   interp=(bash /work/job) ;;
              *) echo "unsupported language: $lang" >&2; exit 2 ;;
            esac

            # --unshare-all drops every namespace, then --share-net puts the
            # network back: scripts can fetch their own data. Only /nix/store
            # and the scratch dir are visible, so there is no path to
            # /var/lib/merlin, /run/secrets or anything else on the host.
            exec timeout --signal=KILL "$timeout_s" \
              bwrap \
                --unshare-all --share-net \
                --die-with-parent \
                --new-session \
                --ro-bind /nix/store /nix/store \
                --ro-bind-try /etc/ssl /etc/ssl \
                --ro-bind-try /etc/static/ssl /etc/static/ssl \
                --ro-bind-try /etc/pki /etc/pki \
                --ro-bind-try /etc/resolv.conf /etc/resolv.conf \
                --ro-bind-try /etc/hosts /etc/hosts \
                --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf \
                --ro-bind-try /etc/services /etc/services \
                --ro-bind-try /etc/protocols /etc/protocols \
                --proc /proc \
                --dev /dev \
                --tmpfs /tmp \
                --bind "$work" /work \
                --chdir /work \
                --setenv HOME /work \
                --setenv PATH /usr/bin:/bin \
                --setenv SSL_CERT_FILE /etc/ssl/certs/ca-certificates.crt \
                --ro-bind "$(dirname "$(readlink -f "$(command -v python3)")")" /usr/bin \
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
