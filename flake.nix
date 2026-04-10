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
          version = "0.3.4";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.installShellFiles ];
          nativeCheckInputs = [ pkgs.git ];
          postInstall = ''
            installShellCompletion --bash target/*/build/tam-cli-*/out/completions/tam.bash
            installShellCompletion --zsh --name _tam target/*/build/tam-cli-*/out/completions/_tam
            installShellCompletion --fish target/*/build/tam-cli-*/out/completions/tam.fish
            installManPage target/*/build/tam-cli-*/out/man/tam.1
          '';
        };
      }
    );
}
