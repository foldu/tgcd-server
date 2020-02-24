let
  pkgs = import <nixpkgs> {};
in
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    protobuf
  ];
  buildInputs = [];
  PROTOC = "${pkgs.protobuf}/bin/protoc";
}
