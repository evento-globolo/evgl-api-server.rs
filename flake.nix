{
  description = "Rust development shell";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let system = "x86_64-linux"; pkgs = import nixpkgs { inherit system; };
    in { devShells.${system}.default = pkgs.mkShell { packages = with pkgs; [ rustup pkg-config openssl postgresql nodejs_22 ]; }; };
  # encrypted env files — env/enc/*.env.enc, see env/README.md
  sops
  age
  python3
  just
}
