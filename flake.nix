{
  description = "Ninja compatible incremental C/C++ build system with Nix ";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    nix = {
      # Local checkout with PR #13768 (varlink builder IPC) for development.
      # Revert to `github:NixOS/nix` once the PR lands.
      url = "git+file:///home/amaanq/projects/nix/varlink?ref=varlink-pr-13768";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.nixpkgs-23-11.follows = "";
      inputs.nixpkgs-regression.follows = "";
    };
    globset = {
      url = "github:pdtpartners/globset";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];
      imports = [ ./modules ];
      flake = { inherit (inputs.nixpkgs) lib; };
    };
}
