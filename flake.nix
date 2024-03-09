{
  description = "Flake exposing various programming tools and compilers";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.esoteric_nix.url = "github:0rvar/esoteric_nix";

  outputs = { self, nixpkgs, esoteric_nix }:
    let
      forAllSystems = function:
        nixpkgs.lib.genAttrs [
          "x86_64-linux"
          "aarch64-linux"
          "x86_64-darwin"
          "aarch64-darwin"
        ]
          (system:
            (function (import nixpkgs {
              inherit system;
            })) system);

      toolsFromFile = path: pkgs: toolPkgs:
        let
          lib = pkgs.lib;
          names = lib.pipe path [
            builtins.readFile
            (lib.splitString "\n")
            (lib.filter (x: x != ""))
            (lib.filter (x: builtins.substring 0 1 x != "#"))
          ];
          toolList = map
            (name: {
              name = name;
              value = toolPkgs.${name};
            })
            names;
          packages = builtins.listToAttrs toolList;
          paths = builtins.attrValues packages;
        in
        {
          packages = packages;
          paths = paths;
        };
    in
    {
      # Dynamically generate a shell for each system
      # containing all the packages in the overlay
      packages = forAllSystems
        (pkgs: system:
          let
            lib = nixpkgs.lib;
            tools = toolsFromFile ./tools.txt pkgs pkgs;
            haskellTools = toolsFromFile ./tools.haskellPackages.txt pkgs pkgs.haskellPackages;
            esotericPackages = esoteric_nix.packages.${system};
            esotericPaths = builtins.attrValues esotericPackages;
          in
          tools.packages // haskellTools.packages // esotericPackages //
          {
            default = pkgs.buildEnv
              {
                name = "all-tools-shell";
                paths = tools.paths ++ haskellTools.paths ++ esotericPaths;
                ignoreCollisions = true;
              };
          }
        );
    };
}
