<#
.SYNOPSIS
  Lado Windows da medição entre máquinas: sobe o destino (contact) e o relay B.

.DESCRIPTION
  Roda na máquina Windows. Imprime uma linha "addr,onion,link"; copie-a para a máquina
  Linux e rode lá: scripts/measure.sh <IP-DESTA-MAQUINA> '<linha>'
  Ctrl+C encerra os dois processos.

.PARAMETER PublicIp
  IP desta máquina, alcançável pela outra (rede local ou pública).

.PARAMETER OpenFirewall
  Cria regras de entrada no Firewall do Windows para as duas portas (precisa de PowerShell
  como administrador). As regras são removidas ao sair.

.EXAMPLE
  .\scripts\measure-windows.ps1 -PublicIp 192.168.0.20 -OpenFirewall
#>
param(
    [Parameter(Mandatory = $true)][string]$PublicIp,
    [int]$RelayPort = 4001,
    [int]$ContactPort = 9000,
    [switch]$OpenFirewall
)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

cargo build --release --features net --example measure
if ($LASTEXITCODE -ne 0) { throw 'cargo build falhou' }
$measure = Join-Path (Get-Location) 'target\release\examples\measure.exe'

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("clam-measure-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
$rules = @()
$procs = @()
try {
    if ($OpenFirewall) {
        foreach ($port in @($RelayPort, $ContactPort)) {
            $name = "clam-measure-$port"
            New-NetFirewallRule -DisplayName $name -Direction Inbound -Protocol TCP -LocalPort $port -Action Allow | Out-Null
            $rules += $name
        }
    }

    # Destino: ecoa os quadros. O relay B (nesta máquina) fala com ele por loopback.
    $procs += Start-Process -FilePath $measure -ArgumentList @('contact', "0.0.0.0:$ContactPort") `
        -RedirectStandardOutput "$tmp\contact.out" -RedirectStandardError "$tmp\contact.err" -PassThru -WindowStyle Hidden

    $procs += Start-Process -FilePath $measure `
        -ArgumentList @('relay', "0.0.0.0:$RelayPort", "${PublicIp}:$RelayPort", "contato:9=127.0.0.1:$ContactPort") `
        -RedirectStandardOutput "$tmp\relay.out" -RedirectStandardError "$tmp\relay.err" -PassThru -WindowStyle Hidden

    $line = $null
    for ($i = 0; $i -lt 50 -and -not $line; $i++) {
        Start-Sleep -Milliseconds 100
        if (Test-Path "$tmp\relay.out") { $line = (Get-Content "$tmp\relay.out" -TotalCount 1) }
        if ($procs[1].HasExited) { throw ("relay caiu: " + (Get-Content "$tmp\relay.err" -Raw)) }
    }
    if (-not $line) { throw 'o relay não imprimiu a linha' }

    Write-Host ''
    Write-Host 'Na máquina Linux, rode:'
    Write-Host "  scripts/measure.sh $PublicIp '$line' 60 $ContactPort"
    Write-Host ''
    Write-Host 'Ctrl+C encerra o destino e o relay.'
    Wait-Process -Id $procs[1].Id
}
finally {
    foreach ($p in $procs) { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
    foreach ($r in $rules) { Remove-NetFirewallRule -DisplayName $r -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
