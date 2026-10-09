{
  description = "Run OpenAI Codex as a named background subagent";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # Linux only: process tracking uses /proc and pidfds.
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "ception";
            version = (lib.importTOML ./Cargo.toml).package.version;

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
                ./SKILL.md
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.makeWrapper ];

            # The check phase runs the integration suite against the fake
            # app-server, which is a test fixture and not shipped. npx on
            # PATH for the default codex command (npx -y @openai/codex);
            # suffixed so the user's own node toolchain wins.
            postInstall = ''
              rm $out/bin/ception-fake-appserver
              wrapProgram $out/bin/ception \
                --suffix PATH : ${lib.makeBinPath [ pkgs.nodejs ]}
            '';

            meta = {
              description = "Run OpenAI Codex as a named background subagent";
              license = lib.licenses.asl20;
              platforms = lib.platforms.linux;
              mainProgram = "ception";
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
            ];
          };
        }
      );
    };
}
