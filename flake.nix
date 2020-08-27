{
  description = "A tagging server thing.";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nmattia/naersk";
  };

  outputs = { self, nixpkgs, naersk, flake-utils }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs { inherit system; };
      in
        {
          defaultPackage = naersk.lib.${system}.buildPackage {
            src = ./.;
            buildInputs = [
              pkgs.protobuf
            ];
            PROTOC = "${pkgs.protobuf}/bin/protoc";
          };
          defaultApp = {
            type = "app";
            program = "${self.defaultPackage.${system}}/bin/tgcd-server";
          };
        }
    );
}
