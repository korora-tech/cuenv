{
  description = "cuenv - A direnv alternative that uses CUE files for environment configuration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self
    , nixpkgs
    , rust-overlay
    , flake-utils
    , treefmt-nix
    ,
    }:
    (flake-utils.lib.eachDefaultSystem
      (
        system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            inherit system overlays;
          };

          # Platform-specific dependencies
          platformDeps =
            with pkgs;
            if stdenv.isDarwin then
              [
                darwin.apple_sdk.frameworks.Security
                darwin.apple_sdk.frameworks.CoreFoundation
              ]
            else if stdenv.isLinux then
              [
                glibc
              ]
            else
              [ ];

          # Build dependencies
          buildInputs =
            with pkgs;
            [
              go_1_24
              openssl
              pkg-config
            ]
            ++ platformDeps;

          # Native build dependencies
          nativeBuildInputs = with pkgs; [
            rust-bin.stable."1.88.0".default
            pkg-config
            gcc
            go_1_24
          ];

          # treefmt configuration
          treefmt = treefmt-nix.lib.evalModule pkgs {
            projectRootFile = "flake.nix";
            programs = {
              # Nix formatter
              nixpkgs-fmt.enable = true;

              cue.enable = true;

              # Rust formatter
              rustfmt = {
                enable = true;
                # Use same edition as Cargo.toml
                edition = "2021";
              };

              # Go formatter
              gofmt.enable = true;

              # YAML formatter
              yamlfmt.enable = true;

              # Prettier formatter - handles markdown with frontmatter support
              prettier = {
                enable = true;
                # Only format markdown files with prettier
                includes = [
                  "*.md"
                  "*.mdx"
                ];
              };

              # TOML formatter
              taplo.enable = true;
            };
          };

          # Development tools
          devTools = with pkgs; [
            cargo-watch
            cargo-edit
            cargo-outdated
            cargo-audit
            cue
            gopls
            gotools
            rust-analyzer
            treefmt.config.build.wrapper
          ];

          # Read version from Cargo.toml
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          version = cargoToml.package.version;

          # Vendor Go dependencies
          goVendor = pkgs.stdenv.mkDerivation {
            pname = "cuenv-go-vendor";
            version = version;
            src = ./libcue-bridge;

            nativeBuildInputs = [ pkgs.go_1_24 ];

            buildPhase = ''
              export HOME=$(mktemp -d)
              export GOPATH="$HOME/go"
              export GOCACHE="$HOME/go-cache"

              go mod vendor
            '';

            installPhase = ''
              mkdir -p $out
              cp -r vendor $out/
            '';

            outputHashMode = "recursive";
            outputHashAlgo = "sha256";
            outputHash = "sha256-l/urNOAK9q5nflBt2ovfbaM3WNCn0ouZlc9RJq/+eKk=";
          };

          cuenv = pkgs.rustPlatform.buildRustPackage {
            pname = "cuenv";
            version = version;

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            # Set up build environment
            preBuild = ''
              export HOME=$(mktemp -d)
              export GOPATH="$HOME/go"
              export GOCACHE="$HOME/go-cache"
              export CGO_ENABLED=1

              # Copy vendored dependencies
              cp -r ${goVendor}/vendor libcue-bridge/
              chmod -R u+w libcue-bridge
            '';

            inherit buildInputs nativeBuildInputs;

            # Platform-specific linker flags
            RUSTFLAGS =
              if pkgs.stdenv.isDarwin then
                "-C link-arg=-framework -C link-arg=Security -C link-arg=-framework -C link-arg=CoreFoundation"
              else
                "";

            # Ensure Go is available during build
            CGO_ENABLED = "1";
            GO = "${pkgs.go_1_24}/bin/go";

            checkPhase = ''
              runHook preCheck
              # Skip tests that require network access or special setup
              cargo test --offline
              runHook postCheck
            '';

            meta = with pkgs.lib; {
              description = "A direnv alternative that uses CUE files for environment configuration";
              homepage = "https://github.com/rawkode/cuenv";
              license = licenses.mit;
              maintainers = [ ];
            };
          };

        in
        {
          packages = {
            default = cuenv;
            cuenv = cuenv;
          };

          # Comprehensive checks for nix flake check
          checks = {
            # Formatting check
            formatting = treefmt.config.build.check self;

            # Build check - ensure the package builds
            build = cuenv;

            # Clippy check
            clippy = pkgs.rustPlatform.buildRustPackage {
              pname = "cuenv-clippy";
              version = version;
              src = ./.;

              cargoLock = {
                lockFile = ./Cargo.lock;
              };

              preBuild = ''
                export HOME=$(mktemp -d)
                export GOPATH="$HOME/go"
                export GOCACHE="$HOME/go-cache"
                export CGO_ENABLED=1
                
                # Copy vendored dependencies
                cp -r ${goVendor}/vendor libcue-bridge/
                chmod -R u+w libcue-bridge
              '';

              inherit buildInputs nativeBuildInputs;

              RUSTFLAGS =
                if pkgs.stdenv.isDarwin then
                  "-C link-arg=-framework -C link-arg=Security -C link-arg=-framework -C link-arg=CoreFoundation"
                else
                  "";

              CGO_ENABLED = "1";
              GO = "${pkgs.go_1_24}/bin/go";

              buildPhase = ''
                cargo clippy --all-targets --all-features -- -D warnings
              '';

              installPhase = ''
                touch $out
              '';
            };

            # Test check
            tests = pkgs.rustPlatform.buildRustPackage {
              pname = "cuenv-tests";
              version = version;
              src = ./.;

              cargoLock = {
                lockFile = ./Cargo.lock;
              };

              preBuild = ''
                export HOME=$(mktemp -d)
                export GOPATH="$HOME/go"
                export GOCACHE="$HOME/go-cache"
                export CGO_ENABLED=1
                
                # Copy vendored dependencies
                cp -r ${goVendor}/vendor libcue-bridge/
                chmod -R u+w libcue-bridge
              '';

              inherit buildInputs nativeBuildInputs;

              RUSTFLAGS =
                if pkgs.stdenv.isDarwin then
                  "-C link-arg=-framework -C link-arg=Security -C link-arg=-framework -C link-arg=CoreFoundation"
                else
                  "";

              CGO_ENABLED = "1";
              GO = "${pkgs.go_1_24}/bin/go";

              checkPhase = ''
                cargo test --offline
              '';

              installPhase = ''
                touch $out
              '';
            };
          };

          # Make formatter available
          formatter = treefmt.config.build.wrapper;

          devShells.default = pkgs.mkShell {
            buildInputs = buildInputs ++ nativeBuildInputs ++ devTools;

            shellHook = ''
              echo "cuenv development environment"
              echo "Rust version: $(rustc --version)"
              echo "Go version: $(go version)"
              echo ""
              echo "Available commands:"
              echo "  cargo build    - Build the project"
              echo "  cargo test     - Run tests"
              echo "  cargo run      - Run cuenv"
              echo "  cargo watch    - Watch for changes and rebuild"
              echo "  treefmt        - Format all code"
              echo "  nix flake check - Check code formatting"
              echo ""

              # Set up environment for building
              export CGO_ENABLED=1
              export GOPATH="$HOME/go"
              export GOCACHE="$HOME/.cache/go-build"

              # Platform-specific setup
              ${
                if pkgs.stdenv.isDarwin then
                  ''
                    export RUSTFLAGS="-C link-arg=-framework -C link-arg=Security -C link-arg=-framework -C link-arg=CoreFoundation"
                  ''
                else
                  ""
              }
            '';
          };
        }
      )) // {
      # Home Manager module  
      homeManagerModules = {
        default = { config, lib, pkgs, ... }:
          with lib;
          let
            cfg = config.programs.cuenv;
          in
          {
            options.programs.cuenv = {
              enable = mkEnableOption "cuenv, a direnv alternative using CUE files";

              package = mkOption {
                type = types.package;
                default = self.packages.${pkgs.system}.default;
                defaultText = literalExpression "cuenv";
                description = "The cuenv package to use.";
              };

              enableBashIntegration = mkOption {
                type = types.bool;
                default = config.programs.bash.enable;
                defaultText = literalExpression "config.programs.bash.enable";
                description = ''
                  Whether to enable Bash integration.
                '';
              };

              enableZshIntegration = mkOption {
                type = types.bool;
                default = config.programs.zsh.enable;
                defaultText = literalExpression "config.programs.zsh.enable";
                description = ''
                  Whether to enable Zsh integration.
                '';
              };

              enableFishIntegration = mkOption {
                type = types.bool;
                default = config.programs.fish.enable;
                defaultText = literalExpression "config.programs.fish.enable";
                description = ''
                  Whether to enable Fish integration.
                '';
              };

              enableNushellIntegration = mkOption {
                type = types.bool;
                default = config.programs.nushell.enable;
                defaultText = literalExpression "config.programs.nushell.enable";
                description = ''
                  Whether to enable Nushell integration.
                
                  Note: Nushell support is experimental and may require manual configuration.
                '';
              };
            };

            config = mkIf cfg.enable {
              home.packages = [ cfg.package ];

              programs.bash.initExtra = mkIf cfg.enableBashIntegration ''
                # cuenv shell integration
                eval "$(${cfg.package}/bin/cuenv init bash)"
              '';

              programs.zsh.initExtra = mkIf cfg.enableZshIntegration ''
                # cuenv shell integration
                eval "$(${cfg.package}/bin/cuenv init zsh)"
              '';

              programs.fish.interactiveShellInit = mkIf cfg.enableFishIntegration ''
                # cuenv shell integration
                ${cfg.package}/bin/cuenv init fish | source
              '';

              programs.nushell.extraConfig = mkIf cfg.enableNushellIntegration ''
                # cuenv shell integration
                # Note: This is experimental and may need adjustment based on your Nushell version
                let cuenv_init = (${cfg.package}/bin/cuenv init nushell | str trim)
                if not ($cuenv_init | is-empty) {
                  source-env { $cuenv_init | from nuon }
                }
              '';
            };
          };
      };
    };
}
