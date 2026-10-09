{
  description = "Run OpenAI Codex as a named background subagent from Claude Code";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # Linux only: session and lock identity come from /proc.
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
          inherit (pkgs) lib nodejs;
        in
        {
          default = pkgs.stdenvNoCC.mkDerivation {
            pname = "ception";
            version = (lib.importJSON ./package.json).version;

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./bin
                ./lib
                ./test
                ./SKILL.md
                ./package.json
              ];
            };

            nativeBuildInputs = [ pkgs.makeWrapper ];
            nativeCheckInputs = [ nodejs ];

            dontBuild = true;
            doCheck = true;
            checkPhase = ''
              runHook preCheck
              node --test test/
              runHook postCheck
            '';

            # npx on PATH for the default codex command (npx -y @openai/codex);
            # suffixed so the user's own node toolchain wins.
            installPhase = ''
              runHook preInstall
              mkdir -p $out/lib/ception
              cp -r bin lib SKILL.md package.json $out/lib/ception/
              makeWrapper ${lib.getExe nodejs} $out/bin/ception \
                --add-flags $out/lib/ception/bin/ception.mjs \
                --suffix PATH : ${lib.makeBinPath [ nodejs ]}
              runHook postInstall
            '';

            meta = {
              description = "Run OpenAI Codex as a named background subagent from Claude Code";
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
              nodejs
            ];
          };
        }
      );
    };
}
