{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nixscroll.url = "git+https://forge.example.invalid/example-nix/nixscroll.git?allRefs=1";
    nixscroll.inputs.nixhost.url = "git+https://forge.example.invalid/example-nix/nixhost.git?allRefs=1";
  };
}
