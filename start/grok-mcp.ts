#!/usr/bin/env bun
/**
 * Servidor MCP que expone Grok como herramienta, hablando con el proxy local
 * (claude-code-proxy) por su API Anthropic-compatible.
 *
 * Protocolo MCP sobre stdio, JSON-RPC 2.0 a mano: sin dependencias, sin
 * node_modules, nada que se actualice y rompa.
 *
 * Requiere el proxy corriendo:  claude-code-proxy serve --no-monitor
 */

const PORT = Number(process.env.GROK_MCP_PORT ?? 18765)
const ENDPOINT = `http://127.0.0.1:${PORT}/v1/messages`
const MODELS = ['grok-4.5', 'grok-composer-2.5-fast'] as const
const DEFAULT_MODEL: (typeof MODELS)[number] = 'grok-4.5'
const DEFAULT_MAX_TOKENS = 16384
// El proxy corta a los 120s por request; damos margen y fallamos con un
// mensaje propio antes de que el socket muera sin explicacion.
const REQUEST_TIMEOUT_MS = 600_000

type JsonRpcId = string | number | null

interface JsonRpcRequest {
  jsonrpc: '2.0'
  id?: JsonRpcId
  method: string
  params?: Record<string, unknown>
}

const TOOLS = [
  {
    name: 'generate',
    description:
      'Genera texto con Grok a traves del proxy local. Pensado para redaccion ' +
      'de largo aliento (escenas, dialogos, escaletas) donde queres la voz de ' +
      'Grok en lugar de la propia. Devuelve el texto crudo.',
    inputSchema: {
      type: 'object',
      properties: {
        prompt: {
          type: 'string',
          description: 'La consigna. Se manda como mensaje de usuario.',
        },
        system: {
          type: 'string',
          description:
            'System prompt: voz, formato, restricciones. Opcional pero muy ' +
            'recomendado para mantener consistencia entre llamadas.',
        },
        model: {
          type: 'string',
          enum: [...MODELS],
          description: `Modelo. Default: ${DEFAULT_MODEL}.`,
        },
        max_tokens: {
          type: 'integer',
          minimum: 1,
          maximum: 200000,
          description: `Tope de tokens de salida. Default: ${DEFAULT_MAX_TOKENS}.`,
        },
        temperature: {
          type: 'number',
          minimum: 0,
          maximum: 2,
          description: '0 deterministico, 1 variado. Omitir usa el default del modelo.',
        },
      },
      required: ['prompt'],
      additionalProperties: false,
    },
  },
  {
    name: 'status',
    description:
      'Verifica que el proxy este levantado y responda. Usar cuando generate ' +
      'falla, para distinguir proxy caido de error de autenticacion.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
] as const

interface AnthropicContentBlock {
  type: string
  text?: string
}

interface AnthropicResponse {
  content?: AnthropicContentBlock[]
  stop_reason?: string
  usage?: { input_tokens?: number; output_tokens?: number }
  error?: { message?: string }
}

async function callGrok(args: Record<string, unknown>): Promise<string> {
  const prompt = args.prompt
  if (typeof prompt !== 'string' || prompt.trim() === '') {
    throw new Error('prompt es obligatorio y no puede estar vacio')
  }

  const model = (args.model as string) ?? DEFAULT_MODEL
  if (!MODELS.includes(model as (typeof MODELS)[number])) {
    throw new Error(`Modelo no soportado: ${model}. Opciones: ${MODELS.join(', ')}`)
  }

  // El proxy rechaza con 400 cualquier campo fuera de su lista blanca, asi que
  // se manda unicamente lo que sabemos que acepta.
  const body: Record<string, unknown> = {
    model,
    max_tokens: (args.max_tokens as number) ?? DEFAULT_MAX_TOKENS,
    stream: false,
    messages: [{ role: 'user', content: prompt }],
  }
  if (typeof args.system === 'string' && args.system !== '') body.system = args.system
  if (typeof args.temperature === 'number') body.temperature = args.temperature

  let response: Response
  try {
    response = await fetch(ENDPOINT, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        'anthropic-version': '2023-06-01',
        'x-api-key': 'unused',
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    })
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error)
    throw new Error(
      `No se pudo contactar al proxy en ${ENDPOINT} (${reason}). ` +
        'Levantalo con: claude-code-proxy serve --no-monitor',
    )
  }

  const raw = await response.text()
  let payload: AnthropicResponse
  try {
    payload = JSON.parse(raw) as AnthropicResponse
  } catch {
    throw new Error(`Respuesta no-JSON del proxy (HTTP ${response.status}): ${raw.slice(0, 500)}`)
  }

  if (!response.ok) {
    throw new Error(
      `El proxy devolvio HTTP ${response.status}: ${payload.error?.message ?? raw.slice(0, 500)}`,
    )
  }

  const text = (payload.content ?? [])
    .filter((block) => block.type === 'text')
    .map((block) => block.text ?? '')
    .join('')

  if (text === '') {
    throw new Error(`Grok no devolvio texto (stop_reason: ${payload.stop_reason ?? 'desconocido'})`)
  }
  return text
}

async function checkStatus(): Promise<string> {
  try {
    const response = await fetch(`http://127.0.0.1:${PORT}/healthz`, {
      signal: AbortSignal.timeout(5000),
    })
    return response.ok
      ? `Proxy activo en 127.0.0.1:${PORT}.`
      : `El proxy respondio HTTP ${response.status} en /healthz.`
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error)
    return (
      `Proxy no accesible en 127.0.0.1:${PORT} (${reason}). ` +
      'Levantalo con: claude-code-proxy serve --no-monitor'
    )
  }
}

function send(message: Record<string, unknown>): void {
  process.stdout.write(`${JSON.stringify(message)}\n`)
}

function reply(id: JsonRpcId, result: unknown): void {
  send({ jsonrpc: '2.0', id, result })
}

function replyError(id: JsonRpcId, code: number, message: string): void {
  send({ jsonrpc: '2.0', id, error: { code, message } })
}

async function handle(request: JsonRpcRequest): Promise<void> {
  const { id, method, params } = request

  switch (method) {
    case 'initialize':
      reply(id ?? null, {
        protocolVersion: '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: { name: 'grok', version: '1.0.0' },
      })
      return

    // Notificaciones: no llevan id y no se responden.
    case 'notifications/initialized':
    case 'notifications/cancelled':
      return

    case 'tools/list':
      reply(id ?? null, { tools: TOOLS })
      return

    case 'tools/call': {
      const name = params?.name as string
      const args = (params?.arguments as Record<string, unknown>) ?? {}
      if (name !== 'generate' && name !== 'status') {
        replyError(id ?? null, -32602, `Herramienta desconocida: ${name}`)
        return
      }
      try {
        const text = name === 'status' ? await checkStatus() : await callGrok(args)
        reply(id ?? null, { content: [{ type: 'text', text }] })
      } catch (error) {
        // Los errores de herramienta van como resultado con isError, no como
        // error de JSON-RPC: asi el modelo los ve y puede reaccionar.
        reply(id ?? null, {
          content: [{ type: 'text', text: error instanceof Error ? error.message : String(error) }],
          isError: true,
        })
      }
      return
    }

    default:
      if (id !== undefined && id !== null) {
        replyError(id, -32601, `Metodo no implementado: ${method}`)
      }
  }
}

// Lectura de stdio delimitada por lineas (newline-delimited JSON).
let buffer = ''
for await (const chunk of process.stdin) {
  buffer += new TextDecoder().decode(chunk as Uint8Array)
  let newline: number
  while ((newline = buffer.indexOf('\n')) !== -1) {
    const line = buffer.slice(0, newline).trim()
    buffer = buffer.slice(newline + 1)
    if (line === '') continue
    try {
      await handle(JSON.parse(line) as JsonRpcRequest)
    } catch (error) {
      process.stderr.write(`grok-mcp: linea invalida: ${String(error)}\n`)
    }
  }
}
