<#
.SYNOPSIS
    Manda un prompt a Grok a traves del proxy local y devuelve el texto.

.DESCRIPTION
    Habla con la API Anthropic-compatible que expone claude-code-proxy en
    127.0.0.1:18765. Requiere que el proxy este corriendo:
        claude-code-proxy serve --no-monitor

.EXAMPLE
    .\grok.ps1 "Escribi una escena de dos paginas entre Marta y el inspector."

.EXAMPLE
    .\grok.ps1 -File .\escaleta.md -System "Sos guionista de novela rioplatense." -Out .\cap03.md

.EXAMPLE
    "Dame tres finales alternativos" | .\grok.ps1 -Model grok-composer-2.5-fast
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromPipeline = $true)]
    [string]$Prompt,

    # Lee el prompt desde un archivo en lugar del argumento.
    [string]$File,

    # System prompt. Define voz, formato y restricciones.
    [string]$System,

    [ValidateSet('grok-4.5', 'grok-composer-2.5-fast')]
    [string]$Model = 'grok-4.5',

    [ValidateRange(1, 200000)]
    [int]$MaxTokens = 16384,

    # 0 = deterministico, 1 = maxima variacion. Omitir usa el default del modelo.
    [ValidateRange(0.0, 2.0)]
    [Nullable[double]]$Temperature,

    # Guarda la respuesta en un archivo (UTF-8) ademas de imprimirla.
    [string]$Out,

    [int]$Port = 18765
)

$ErrorActionPreference = 'Stop'

if ($File) {
    if (-not (Test-Path -LiteralPath $File)) {
        throw "No existe el archivo: $File"
    }
    $Prompt = Get-Content -LiteralPath $File -Raw -Encoding UTF8
}

if ([string]::IsNullOrWhiteSpace($Prompt)) {
    throw 'Falta el prompt. Pasalo como argumento, por -File, o por pipeline.'
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

# UTF-8 explicito en los dos sentidos: los acentos y la enie se rompen si se
# deja que PowerShell elija la codificacion.
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
    # En PowerShell 7 el cuerpo de una respuesta de error llega por
    # ErrorDetails; .Exception.Response es un HttpResponseMessage y no tiene
    # GetResponseStream(). Sin esto, un 502 del proxy se confundia con "el
    # proxy no esta levantado", que es un diagnostico completamente distinto.
    $detail = $_.ErrorDetails.Message
    $status = $null
    if ($_.Exception.PSObject.Properties.Name -contains 'Response' -and $_.Exception.Response) {
        $status = [int]$_.Exception.Response.StatusCode
    }

    if ($detail) {
        $mensaje = $null
        try { $mensaje = ($detail | ConvertFrom-Json).error.message } catch { }
        if (-not $mensaje) { $mensaje = $detail }
        throw "El proxy respondio HTTP $status : $mensaje"
    }

    if ($status) {
        throw "El proxy respondio HTTP $status sin cuerpo."
    }

    throw "No se pudo conectar a $uri ($($_.Exception.Message)). Levanta el proxy con: claude-code-proxy serve --no-monitor"
}

# Se decodifica desde el stream crudo: PowerShell 7 entrega .Content ya como
# texto y 5.1 a veces como byte[], asi que no se puede asumir ninguno de los dos.
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
    Write-Warning "Grok no devolvio texto (stop_reason: $($payload.stop_reason))."
}

if ($Out) {
    $dir = Split-Path -Parent $Out
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
    Set-Content -LiteralPath $Out -Value $text -Encoding UTF8
    Write-Verbose "Guardado en $Out"
}

Write-Verbose ("tokens: entrada={0} salida={1} stop={2}" -f `
        $payload.usage.input_tokens, $payload.usage.output_tokens, $payload.stop_reason)

$text
