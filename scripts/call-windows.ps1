<#
.SYNOPSIS
  Lado Windows da chamada de teste: sobe o telefone (quem atende) e o relay B.

.DESCRIPTION
  Roda na máquina Windows. Antes, na máquina Linux: scripts/call.sh pub  (imprime a chave
  pública de quem liga; passe em -CallerPub). O script imprime o comando para rodar no Linux.
  Ctrl+C encerra os processos e remove as regras de firewall.

.PARAMETER PublicIp
  IP desta máquina, alcançável pelo Linux.

.PARAMETER CallerPub
  Chave pública (64 hex) de quem vai ligar. Só ela toca neste telefone.

.PARAMETER OpenFirewall
  Cria regras de entrada para as duas portas (PowerShell como administrador).

.EXAMPLE
  .\scripts\call-windows.ps1 -PublicIp 192.168.0.20 -CallerPub ab12... -OpenFirewall
#>
param(
    [Parameter(Mandatory = $true)][string]$PublicIp,
    [Parameter(Mandatory = $true)][string]$CallerPub,
    [int]$RelayPort = 4001,
    [int]$PhonePort = 7000,
    [switch]$OpenFirewall
)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

cargo build --release --features net --example call --example measure
if ($LASTEXITCODE -ne 0) { throw 'cargo build falhou' }
$call = Join-Path (Get-Location) 'target\release\examples\call.exe'
$measure = Join-Path (Get-Location) 'target\release\examples\measure.exe'
$key = Join-Path (Get-Location) 'call-identity.key'

$phonePub = (& $call keygen $key | Select-Object -Last 1).Trim()

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("clam-call-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
$rules = @()
$procs = @()
try {
    if ($OpenFirewall) {
        foreach ($port in @($RelayPort, $PhonePort)) {
            $name = "clam-call-$port"
            New-NetFirewallRule -DisplayName $name -Direction Inbound -Protocol TCP -LocalPort $port -Action Allow | Out-Null
            $rules += $name
        }
    }

    # Telefone: o relay B (esta máquina) chega nele por loopback.
    $procs += Start-Process -FilePath $call -ArgumentList @('phone', "0.0.0.0:$PhonePort", $key, $CallerPub) `
        -RedirectStandardOutput "$tmp\phone.out" -RedirectStandardError "$tmp\phone.err" -PassThru -WindowStyle Hidden

    $procs += Start-Process -FilePath $measure `
        -ArgumentList @('relay', "0.0.0.0:$RelayPort", "${PublicIp}:$RelayPort", "contato:9=127.0.0.1:$PhonePort") `
        -RedirectStandardOutput "$tmp\relay.out" -RedirectStandardError "$tmp\relay.err" -PassThru -WindowStyle Hidden

    $line = $null
    for ($i = 0; $i -lt 50 -and -not $line; $i++) {
        Start-Sleep -Milliseconds 100
        if (Test-Path "$tmp\relay.out") { $line = (Get-Content "$tmp\relay.out" -TotalCount 1) }
        if ($procs[1].HasExited) { throw ("relay caiu: " + (Get-Content "$tmp\relay.err" -Raw)) }
        if ($procs[0].HasExited) { throw ("telefone caiu: " + (Get-Content "$tmp\phone.err" -Raw)) }
    }
    if (-not $line) { throw 'o relay não imprimiu a linha' }

    Write-Host ''
    Write-Host 'Na máquina Linux, rode (troque <IP-LINUX> pelo IP dela, alcançável daqui):'
    Write-Host "  scripts/call.sh dial <IP-LINUX> $PublicIp '$line' $phonePub 15 $PhonePort"
    Write-Host ''
    Write-Host 'Aqui aparece o log do telefone abaixo. Ctrl+C encerra.'
    $seen = 0
    while (-not $procs[0].HasExited) {
        Start-Sleep -Milliseconds 500
        if (Test-Path "$tmp\phone.out") {
            $all = @(Get-Content "$tmp\phone.out")
            if ($all.Count -gt $seen) { $all[$seen..($all.Count - 1)] | ForEach-Object { Write-Host $_ }; $seen = $all.Count }
        }
    }
}
finally {
    foreach ($p in $procs) { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
    foreach ($r in $rules) { Remove-NetFirewallRule -DisplayName $r -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
