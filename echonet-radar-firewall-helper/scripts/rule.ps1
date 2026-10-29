$ErrorActionPreference = 'Stop'

$action = $env:ECHONET_RADAR_FIREWALL_ACTION
if ($action -ne 'add' -and $action -ne 'remove') {
    [Console]::Error.WriteLine('ECHONET_RADAR_FIREWALL_ACTION must be add or remove')
    exit 2
}

$target = $env:ECHONET_RADAR_FIREWALL_TARGET
if ($action -eq 'add' -and [string]::IsNullOrEmpty($target)) {
    [Console]::Error.WriteLine('ECHONET_RADAR_FIREWALL_TARGET is not set')
    exit 1
}

$rules = @(
    @{ Name = 'EchonetRadarUdp3610Private'; Profile = 'Private' },
    @{ Name = 'EchonetRadarUdp3610Domain'; Profile = 'Domain' }
)

foreach ($rule in $rules) {
    Get-NetFirewallRule -Name $rule.Name -ErrorAction SilentlyContinue |
        Remove-NetFirewallRule -ErrorAction SilentlyContinue
}

if ($action -eq 'remove') {
    exit 0
}

foreach ($rule in $rules) {
    New-NetFirewallRule -Name $rule.Name `
        -DisplayName "ECHONET Radar UDP 3610 ($($rule.Profile))" `
        -Program $target `
        -Direction Inbound `
        -Action Allow `
        -Protocol UDP `
        -LocalPort 3610 `
        -Profile $rule.Profile `
        -RemoteAddress LocalSubnet `
        -ErrorAction Stop | Out-Null
}

exit 0
