$ErrorActionPreference = 'Stop'
$envPath = Join-Path (Split-Path $PSScriptRoot -Parent) '.env'

$secure = Read-Host 'New INDstocks access token' -AsSecureString
$pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try { $token = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer) }
if ([string]::IsNullOrWhiteSpace($token) -or $token.Contains("`r") -or $token.Contains("`n")) {
    throw 'Token must be a non-empty single-line value. .env was not changed.'
}

$lines = @()
if (Test-Path $envPath) {
    $lines = [IO.File]::ReadAllLines($envPath) | Where-Object { $_ -notmatch '^INDSTOCKS_ACCESS_TOKEN=' }
}
$lines += "INDSTOCKS_ACCESS_TOKEN=$token"
[IO.File]::WriteAllLines($envPath, [string[]]$lines, [Text.UTF8Encoding]::new($false))
Write-Output 'Updated the INDstocks access token in .env. Other entries were kept.'
