{
  description = "tw-lint — Tailwind LSP-driven linter/fixer";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        server = pkgs.tailwindcss-language-server;
        unwrapped = pkgs.rustPlatform.buildRustPackage {
          pname = "tw-lint";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The e2e tests need a running language server; run only unit tests
          # in the sandbox.
          cargoTestFlags = [ "--lib" ];
        };
        wrapped = pkgs.runCommand "tw-lint"
          { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
          mkdir -p $out/bin
          makeWrapper ${unwrapped}/bin/tw-lint $out/bin/tw-lint \
            --prefix PATH : ${pkgs.lib.makeBinPath [ server ]}
        '';
      in {
        packages.default = wrapped;
        apps.default = {
          type = "app";
          program = "${wrapped}/bin/tw-lint";
        };
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.nodejs
            server
          ];
        };
      });
}
