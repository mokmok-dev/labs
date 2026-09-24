{
  craneLib,
  commonArgs,
  cargoArtifacts,
}:
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
    pname = "echonet-radar-cli";
    cargoExtraArgs = "-p echonet-radar-cli";
  }
)
