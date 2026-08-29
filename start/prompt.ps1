<#
.SYNOPSIS
    Send one prompt through the local proxy and return the text.

.DESCRIPTION
    Talks to the Anthropic-compatible API that claude-code-proxy exposes on
    127.0.0.1:18765. The model ID picks the provider, so any model the proxy
    routes works here: Codex, Kimi, Grok, OpenCode Go or Cursor. The proxy has
    to be running:
        claude-code-proxy serve --no-monitor
    or just double-click start-proxy.cmd next to this file.

.EXAMPLE
    .\prompt.ps1 -Model grok-4.6 "Write a two-page scene between Marta and the inspector."

.EXAMPLE
    .\prompt.ps1 -Model gpt-5.6-sol -File .\outline.md -Out .\ch03.md

.EXAMPLE
    "Give me three alternative endings" | .\prompt.ps1 -Model kimi-k3

.NOTES
    -Model is required unless CCP_MCP_MODEL is set. Run 'claude-code-proxy
    models' for the catalog; an unknown ID comes back with it too.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromPipeline = $true)]
    [string]$Prompt,

    # Read the prompt from a file instead of the argument.
    [string]$File,

    # System prompt. Sets voice, format and constraints.
    [string]$System,

    # Any model ID the proxy routes; it also picks the provider. Not validated
    # here on purpose: the registry is the proxy's, and Cursor's catalog is
    # discovered at runtime, so a list baked in here would drift.
    [string]$Model = $env:CCP_MCP_MODEL,

    [ValidateRange(1, 200000)]
    [int]$MaxTokens = 16384,

    # 0 is deterministic, 1 is maximum variation. Omit to use the model default.
    [ValidateRange(0.0, 2.0)]
    [Nullable[double]]$Temperature,

    # Also write the answer to a file, as UTF-8.
    [string]$Out,

    [int]$Port = 18765
)

$ErrorActionPreference = 'Stop'

if ($File) {
    if (-not (Test-Path -LiteralPath $File)) {
        throw "No such file: $File"
    }
    $Prompt = Get-Content -LiteralPath $File -Raw -Encoding UTF8
}

if ([string]::IsNullOrWhiteSpace($Prompt)) {
    throw 'No prompt. Pass it as an argument, through -File, or on the pipeline.'
}

if ([string]::IsNullOrWhiteSpace($Model)) {
    throw "No model. Pass -Model, or set CCP_MCP_MODEL. Run 'claude-code-proxy models' for the catalog."
}

$body = [ordered]@{
    model      = $Model
    max_tokens = $MaxTokens
    stream     = $false
    messages   = @(
        [ordered]@{
            role    = 'user'
            content = $Prompt
        }
    )
}

if ($System) { $body.system = $System }
if ($null -ne $Temperature) { $body.temperature = $Temperature }

# UTF-8 explicitly in both directions: accented characters break if PowerShell
# is left to pick the encoding on its own.
$json = $body | ConvertTo-Json -Depth 20 -Compress
$bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
$uri = "http://127.0.0.1:$Port/v1/messages"

try {
    $response = Invoke-WebRequest -Uri $uri -Method Post -Body $bytes `
        -ContentType 'application/json; charset=utf-8' `
        -Headers @{ 'anthropic-version' = '2023-06-01'; 'x-api-key' = 'unused' } `
        -TimeoutSec 600 -UseBasicParsing
}
catch {
    # In PowerShell 7 an error response body arrives through ErrorDetails;
    # .Exception.Response is an HttpResponseMessage and has no
    # GetResponseStream(). Without this, a 502 from the proxy looked like "the
    # proxy is not running", which is a completely different diagnosis.
    $detail = $_.ErrorDetails.Message
    $status = $null
    if ($_.Exception.PSObject.Properties.Name -contains 'Response' -and $_.Exception.Response) {
        $status = [int]$_.Exception.Response.StatusCode
    }

    if ($detail) {
        $message = $null
        try { $message = ($detail | ConvertFrom-Json).error.message } catch { }
        if (-not $message) { $message = $detail }
        throw "The proxy answered HTTP $status : $message"
    }

    if ($status) {
        throw "The proxy answered HTTP $status with no body."
    }

    throw "Could not reach $uri ($($_.Exception.Message)). Start the proxy with: claude-code-proxy serve --no-monitor"
}

# Decoded from the raw stream: PowerShell 7 hands back .Content already as
# text and 5.1 sometimes as byte[], so neither can be assumed.
$raw = if ($response.RawContentStream) {
    [System.Text.Encoding]::UTF8.GetString($response.RawContentStream.ToArray())
}
elseif ($response.Content -is [byte[]]) {
    [System.Text.Encoding]::UTF8.GetString($response.Content)
}
else {
    $response.Content
}

$payload = $raw | ConvertFrom-Json

$text = ($payload.content |
    Where-Object { $_.type -eq 'text' } |
    ForEach-Object { $_.text }) -join ''

if ([string]::IsNullOrEmpty($text)) {
    Write-Warning "The model returned no text (stop_reason: $($payload.stop_reason))."
}

if ($Out) {
    $dir = Split-Path -Parent $Out
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
    Set-Content -LiteralPath $Out -Value $text -Encoding UTF8
    Write-Verbose "Written to $Out"
}

Write-Verbose ("model={0} input={1} output={2} stop={3}" -f `
        $payload.model, $payload.usage.input_tokens, $payload.usage.output_tokens,
        $payload.stop_reason)

$text
