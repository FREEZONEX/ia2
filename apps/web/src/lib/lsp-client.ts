//! Minimal LSP client over WebSocket.
//!
//! Speaks JSON-RPC frames to /api/lsp (proxied through axum to a per-
//! connection ironplc LSP subprocess). Drives the diagnostics flow:
//! initialize → initialized → textDocument/didOpen → didChange on every
//! source update → publishDiagnostics back.

import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"

type LspRange = {
  start: { line: number; character: number }
  end: { line: number; character: number }
}

type LspDiagnostic = {
  range: LspRange
  severity?: number
  code?: string | number
  source?: string
  message: string
}

export type LspClientOptions = {
  uri: string
  languageId: string
  onDiagnostics: (diags: CheckDiagnostic[]) => void
}

/** The LSP receives source text in memory; this URI identifies a logical
 * POU, not a file on the server's disk. Keep it independent of host drive
 * letters, and encode each segment so spaces, Unicode, # and % stay in
 * the document path instead of becoming an invalid URI or a fragment. */
export function pouDocumentUri(path: string): string {
  return `file:///${path.split("/").map(encodeURIComponent).join("/")}.st`
}

function lspToCheck(diag: LspDiagnostic): CheckDiagnostic {
  return {
    severity:
      diag.severity === 2
        ? "warning"
        : diag.severity === 3 || diag.severity === 4
          ? "info"
          : "error",
    code: diag.code != null ? String(diag.code) : "",
    message: diag.message,
    // LSP uses 0-based line/character; Monaco wants 1-based.
    start_line: diag.range.start.line + 1,
    start_column: diag.range.start.character + 1,
    end_line: diag.range.end.line + 1,
    end_column: diag.range.end.character + 1,
    // LSP diagnostics always come from ST sources (Monaco editor),
    // never from graphical-language JSON, so the per-language
    // location fields all stay null on the wire.
    ld_location: null,
    fbd_location: null,
    sfc_location: null,
    // LSP `relatedInformation` and `data` carry the same shape as
    // our `related` / `context` / `explanation` fields, but plumbing
    // them through requires upstream LSP work (Phase 2). For now the
    // HTTP `/api/check` path remains the source of truth for the
    // "thick" diagnostics on ST POUs; LSP markers stay shallow.
    context: [],
    related: [],
    explanation: null,
  }
}

export class LspClient {
  private ws: WebSocket | null = null
  private opts: LspClientOptions
  private initialized = false
  private opened = false
  private version = 0
  private currentSource: string | null = null
  private nextId = 1

  constructor(opts: LspClientOptions) {
    this.opts = opts
    const proto = window.location.protocol === "https:" ? "wss" : "ws"
    const url = `${proto}://${window.location.host}/api/lsp`
    const ws = new WebSocket(url)
    this.ws = ws
    ws.onopen = () => this.initialize()
    ws.onmessage = (ev) => this.onMessage(ev.data)
    ws.onclose = (ev) => {
      this.initialized = false
      this.opened = false
      // An abnormal close with a server-supplied reason means the LSP
      // sidecar couldn't start (incomplete install). Surface it once —
      // the editor itself keeps working; squiggles come from /api/check.
      if (ev.code !== 1000 && ev.reason) {
        console.warn(`LSP unavailable (editor still works): ${ev.reason}`)
      }
    }
    ws.onerror = () => {
      this.initialized = false
      this.opened = false
    }
  }

  /** Push the latest source. Sent immediately if initialized, otherwise
   *  buffered for the post-init flush. */
  setSource(source: string) {
    this.currentSource = source
    this.flushSource()
  }

  dispose() {
    if (this.ws && this.ws.readyState <= WebSocket.OPEN) {
      // Polite shutdown — server already kills the child on WS close.
      try {
        this.ws.close(1000, "client disposed")
      } catch {
        /* ignore */
      }
    }
    this.ws = null
  }

  private initialize() {
    this.send({
      id: this.nextId++,
      method: "initialize",
      params: {
        processId: null,
        rootUri: null,
        capabilities: {},
        initializationOptions: { dialect: "iec61131-3-ed2" },
      },
    })
  }

  private onMessage(data: string) {
    let msg: { id?: number; method?: string; params?: unknown; result?: unknown }
    try {
      msg = JSON.parse(data)
    } catch {
      return
    }
    if (msg.method === "textDocument/publishDiagnostics") {
      const params = msg.params as
        | { uri?: string; diagnostics?: LspDiagnostic[] }
        | undefined
      if (!params || params.uri !== this.opts.uri) return
      this.opts.onDiagnostics((params.diagnostics ?? []).map(lspToCheck))
      return
    }
    if (!this.initialized && msg.id != null && msg.result !== undefined) {
      this.initialized = true
      this.send({ method: "initialized", params: {} })
      this.flushSource()
    }
  }

  private flushSource() {
    if (!this.initialized) return
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return
    const text = this.currentSource ?? ""
    this.version++
    if (!this.opened) {
      this.send({
        method: "textDocument/didOpen",
        params: {
          textDocument: {
            uri: this.opts.uri,
            languageId: this.opts.languageId,
            version: this.version,
            text,
          },
        },
      })
      this.opened = true
    } else {
      this.send({
        method: "textDocument/didChange",
        params: {
          textDocument: { uri: this.opts.uri, version: this.version },
          contentChanges: [{ text }],
        },
      })
    }
  }

  private send(msg: { id?: number; method: string; params: unknown }) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return
    this.ws.send(JSON.stringify({ jsonrpc: "2.0", ...msg }))
  }
}
