# Rust toolchain plus the mingw-w64 linker for cross-building the Windows guest:
#   nix-shell --run 'rustup target add x86_64-pc-windows-gnu'   # once
#   nix-shell --run 'cargo build --release --target x86_64-pc-windows-gnu'
{ pkgs ? import <nixpkgs> { } }:

let
  mingw = pkgs.pkgsCross.mingwW64;
in
pkgs.mkShell {
  packages = [
    pkgs.rustup
    mingw.stdenv.cc
  ];

  CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc.targetPrefix}gcc";
  # Rust's windows-gnu std links -l:libpthread.a. Passed as a path rather than
  # listed in `packages`, which would splice it to the (unsupported) native build.
  CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib";
}
