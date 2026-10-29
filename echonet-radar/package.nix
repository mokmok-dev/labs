{
  cargoArtifacts,
  commonArgs,
  craneLib,
  guiLibraries,
  pkgs,
}:
let
  inherit (pkgs) lib;
  isLinux = pkgs.stdenv.hostPlatform.isLinux;
  isDarwin = pkgs.stdenv.hostPlatform.isDarwin;

  fontsConf = pkgs.makeFontsConf {
    fontDirectories = [ pkgs.ibm-plex ];
  };

  app = "$out/Applications/ECHONET Radar.app";
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
    pname = "echonet-radar";
    # The helper resolves the app as its sibling, so both binaries land in the
    # same bin directory.
    cargoExtraArgs = "-p echonet-radar -p echonet-radar-firewall-helper";

    nativeBuildInputs = commonArgs.nativeBuildInputs ++ lib.optionals isLinux [ pkgs.makeWrapper ];

    postInstall =
      lib.optionalString isLinux ''
        wrapProgram $out/bin/echonet-radar \
          --suffix LD_LIBRARY_PATH : ${lib.makeLibraryPath guiLibraries} \
          --set FONTCONFIG_FILE ${fontsConf}

        install -Dm644 ${./packaging/linux/echonet-radar.desktop} \
          $out/share/applications/echonet-radar.desktop
        install -Dm644 ${./packaging/linux/echonet-radar.svg} \
          $out/share/icons/hicolor/scalable/apps/echonet-radar.svg

        policy=$out/share/polkit-1/actions/dev.mokmok.echonet-radar.policy
        install -Dm644 ${../echonet-radar-firewall-helper/polkit/dev.mokmok.echonet-radar.policy} "$policy"
        substituteInPlace "$policy" \
          --replace-fail /run/current-system/sw/bin/echonet-radar-firewall-helper \
          "$out/bin/echonet-radar-firewall-helper"

        rules=$out/share/polkit-1/rules.d/50-echonet-radar.rules
        install -Dm644 ${./packaging/linux/50-echonet-radar.rules} "$rules"
        substituteInPlace "$rules" \
          --replace-fail @helper@ "$out/bin/echonet-radar-firewall-helper"
      ''
      + lib.optionalString isDarwin ''
        mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
        install -m755 $out/bin/echonet-radar "${app}/Contents/MacOS/echonet-radar"
        install -m644 ${./packaging/macos/Info.plist} "${app}/Contents/Info.plist"
      '';

    meta = {
      description = "Read-only window over the ECHONET Lite LAN";
      homepage = "https://github.com/mokmok-dev/labs";
      mainProgram = "echonet-radar";
      license = lib.licenses.mit;
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  }
)
