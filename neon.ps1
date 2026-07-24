[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet("first-run", "image", "toolchain", "build", "init", "start", "stop", "restart", "status", "logs", "shell", "psql", "reset", "destroy-data")]
    [string]$Command = "status",

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ExtraArgs
)

$ErrorActionPreference = "Stop"
$PinnedCommit = "8f60b04da47ffefe0e52bda2440134b42874eb75"

function Invoke-Compose {
    & docker compose @args
    if ($LASTEXITCODE -ne 0) {
        throw "docker compose failed with exit code $LASTEXITCODE"
    }
}

function Assert-PinnedCommit {
    $actual = (& git rev-parse HEAD).Trim()
    if ($actual -ne $PinnedCommit) {
        throw "Expected Neon commit $PinnedCommit, but HEAD is $actual."
    }
}

function Ensure-Container {
    Invoke-Compose up -d --no-recreate neon
}

function Invoke-Control {
    Ensure-Container
    Invoke-Compose exec neon bash docker/local/neon-control.sh @args
}

switch ($Command) {
    "first-run" {
        Assert-PinnedCommit
        Invoke-Compose build neon
        Ensure-Container
        Invoke-Control toolchain
        Invoke-Control build
        Invoke-Control init
        Invoke-Control start
        Invoke-Compose ps
    }
    "image"       { Assert-PinnedCommit; Invoke-Compose build neon }
    "toolchain"   { Invoke-Control toolchain }
    "build"       { Assert-PinnedCommit; Invoke-Control build }
    "init"        { Assert-PinnedCommit; Invoke-Control init }
    "start"       { Assert-PinnedCommit; Invoke-Control start }
    "stop"        { Invoke-Control stop; Invoke-Compose stop neon }
    "restart"     { Invoke-Control stop; Invoke-Compose stop neon; Invoke-Control start }
    "status"      { Invoke-Compose ps; Invoke-Control status }
    "logs"        { Invoke-Compose logs --tail 200 -f neon }
    "shell"       { Invoke-Control shell }
    "psql"        { Invoke-Control psql @ExtraArgs }
    "reset"       { Invoke-Control stop; Invoke-Compose stop neon }
    "destroy-data" {
        if ($ExtraArgs -notcontains "--confirm") {
            throw "This permanently deletes Neon data. Re-run: .\neon.ps1 destroy-data --confirm"
        }
        Invoke-Compose down
        & docker volume rm neon-local-data
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to remove Docker volume neon-local-data."
        }
    }
}
