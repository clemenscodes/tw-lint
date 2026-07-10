{
  description = "tw-lint — Tailwind LSP-driven linter/fixer";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = import nixpkgs { inherit system; };
      in {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt
            pkgs.nodejs
            pkgs.tailwindcss-language-server
          ];
        };
      });
}
