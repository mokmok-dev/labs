{
  craneLib,
  commonArgs,
  cargoArtifacts,
}:
let
  echonet-radar = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      pname = "echonet-radar";
      cargoExtraArgs = "-p echonet-radar";
    }
  );
in
{
  inherit echonet-radar;
  default = echonet-radar;
}
