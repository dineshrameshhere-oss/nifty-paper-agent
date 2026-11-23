$ErrorActionPreference = 'Stop'
$envPath = Join-Path (Split-Path $PSScriptRoot -Parent) '.env'

function Read-SecretValue([string] $Prompt) {
    $secure = Read-Host $Prompt -AsSecureString
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer) }
}

$accessToken = Read-SecretValue 'INDstocks access token'
$groqKey = Read-SecretValue 'Groq API key'
foreach ($value in @($accessToken, $groqKey)) {
    if ([string]::IsNullOrWhiteSpace($value) -or $value.Contains("`r") -or $value.Contains("`n")) {
        throw 'Keys must be non-empty single-line values. No .env file was written.'
    }
}

$lines = @(
    "INDSTOCKS_ACCESS_TOKEN=$accessToken"
    "GROQ_API_KEY=$groqKey"
)
[IO.File]::WriteAllLines($envPath, [string[]]$lines, [Text.UTF8Encoding]::new($false))
Write-Output 'Saved credentials to .env. Values were not displayed.'
