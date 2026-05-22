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
    #busybox
    #sudo
  ];
  languages.rust.enable = true;
}
