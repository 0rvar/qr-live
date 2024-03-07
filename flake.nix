{
  description = "Flake exposing various programming tools and compilers";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";
  inputs.esoteric_nix.url = "github:0rvar/esoteric_nix";

  outputs = { self, nixpkgs, esoteric_nix, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = nixpkgs.lib;
        tools = lib.pipe ./tools.txt [
          builtins.readFile
          (lib.splitString "\n")
          (lib.filter (x: x != ""))
          (lib.filter (x: builtins.substring 0 1 x != "#"))
          (lib.flip lib.attrVals
            pkgs)
        ];
        esotericPackages = esoteric_nix.packages.${system};
        esoNames = builtins.attrNames esotericPackages;
        esotericTools = map (name: esotericPackages.${name}) esoNames;
      in
      {
        packages.default = pkgs.buildEnv
          {
            name = "all-tools-shell";
            paths = tools ++ esotericTools;
            ignoreCollisions = true;
          };
      }
    );
}
