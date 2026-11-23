$ErrorActionPreference = 'Stop'
$envPath = Join-Path (Split-Path $PSScriptRoot -Parent) '.env'

$secure = Read-Host 'Google Gemini API key' -AsSecureString
$pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try { $key = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer) }
if ([string]::IsNullOrWhiteSpace($key) -or $key.Contains("`r") -or $key.Contains("`n")) {
    throw 'Key must be a non-empty single-line value. .env was not changed.'
}

$lines = @()
if (Test-Path $envPath) {
    $lines = [IO.File]::ReadAllLines($envPath) | Where-Object { $_ -notmatch '^(GEMINI_API_KEY|GROQ_API_KEY)=' }
}
$lines += "GEMINI_API_KEY=$key"
[IO.File]::WriteAllLines($envPath, [string[]]$lines, [Text.UTF8Encoding]::new($false))
Write-Output 'Saved Gemini API key to .env. Values were not displayed.'
