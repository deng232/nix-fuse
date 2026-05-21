{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  packages = with pkgs; [
    git
    pkg-config
    nix.dev
    nsjail
  ];
  languages.rust.enable = true;
}
