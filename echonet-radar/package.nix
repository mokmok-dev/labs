{
  craneLib,
  commonArgs,
  cargoArtifacts,
  lib,
  pkgs,
  webAssets,
}:
let
  # GPUI and wry load these shared libraries at runtime on Linux. macOS ships
  # the needed frameworks with the OS, so no wrapping is required there.
  guiLibs = with pkgs; [
    wayland
    vulkan-loader
    libxkbcommon
    fontconfig
    glib
    gtk3
    webkitgtk_4_1
  ];
  fontsConf = pkgs.makeFontsConf {
    fontDirectories = [ pkgs.ibm-plex ];
  };
  echonet-radar = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      pname = "echonet-radar";
      cargoExtraArgs = "-p echonet-radar";
      nativeBuildInputs =
        commonArgs.nativeBuildInputs ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.makeWrapper ];
      postInstall = lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
        wrapProgram $out/bin/echonet-radar \
          --suffix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath guiLibs} \
          --set FONTCONFIG_FILE ${fontsConf}
      '';
    }
  );
in
{
  inherit echonet-radar webAssets;
  default = echonet-radar;
}
