{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = nixpkgs.legacyPackages.${system}; in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "tam";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.installShellFiles ];
          nativeCheckInputs = [ pkgs.git ];
          postInstall = ''
            installShellCompletion --bash completions/tam.bash
            installShellCompletion --zsh --name _tam completions/tam.zsh
            installShellCompletion --fish completions/tam.fish
          '';
        };
      }
    );
}
