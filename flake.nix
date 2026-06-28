{
  description = "arb — cross-AMM arbitrage scanner (Base / BSC / Tron)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Single source of truth for the toolchain version + components:
        # rust-toolchain.toml.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # C/TLS build deps. The `live-rpc` feature pulls aws-lc-sys (rustls
        # crypto backend), which needs a C compiler + cmake. nixpkgs' wrapped
        # clang knows the macOS SDK sysroot, so the bare-cc "-liconv not found"
        # / SDK problems disappear.
        nativeBuildInputs = [
          pkgs.pkg-config
          pkgs.cmake
        ];
        buildInputs = [ pkgs.libiconv ];

        # Build only the Rust sources (drop READMEs etc.) so dep builds cache
        # across unrelated file changes.
        src = craneLib.cleanCargoSource ./.;

        # Shared by the dep-only build, the package, and every check — one
        # definition, no drift between `nix build` and `nix flake check`.
        commonArgs = {
          inherit src buildInputs nativeBuildInputs;
          strictDeps = true;
        };

        # Compile + cache dependencies once (default feature set: light,
        # network-free). Reused by the package and the clippy/test checks.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Default package: light, network-free build (no `live-rpc`). doCheck
        # runs the default test suite, which is network-free by design.
        arb = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });

        # Deployable variant with real WS streaming. Pulls aws-lc-sys, so it
        # rebuilds its own deps (different feature set => different artifacts).
        arb-live = craneLib.buildPackage (commonArgs // {
          pname = "arb-live";
          cargoExtraArgs = "--features live-rpc";
          # Network-touching tests would run under `live-rpc`; keep the
          # hermetic build to compilation only.
          doCheck = false;
        });
      in
      {
        packages = {
          default = arb;
          arb = arb;
          live = arb-live;
        };

        # `nix flake check` gates: build, default tests, and clippy. Clippy runs
        # without `--deny warnings` since the tree carries some doc-lint
        # warnings today; it still fails on hard errors. `cargo fmt --check` is
        # intentionally omitted because the tree is not fmt-clean yet.
        checks = {
          inherit arb;
          arb-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets";
          });
          arb-test = craneLib.cargoTest (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ rustToolchain ] ++ nativeBuildInputs;
          inherit buildInputs;
          shellHook = ''
            echo "arb devshell: $(rustc --version)"

            # --- local env config (see .env.example) ---
            # Per-chain WS endpoints (with API keys) live in .env, which is
            # gitignored. Copy .env.example -> .env and fill in your endpoints.
            if [ -f .env ]; then
              set -a; . ./.env; set +a
              echo "  env: loaded .env"
            else
              echo "  env: no .env (copy .env.example -> .env to set RPC endpoints)"
            fi

            echo "commands:"
            echo "  cargo test                                  # core suite"
            echo "  cargo run --features live-rpc -- run --chain base --exchange univ2 --pool 0x... --secs 10"
            echo "  cargo run --features live-rpc -- timing-bench --chain base --exchange univ2 --pool 0x... --secs 10"
          '';
        };
      });
}
