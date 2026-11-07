// The `veles` object a JavaScript / TypeScript extension sees — types
// for tooling, never loaded by veles.
//
// The lang-js host binds `veles` before the extension's own code runs
// (crates/veles-lang-js/src/prelude.js is the implementation and the
// contract; docs/plugins.md "JavaScript / TypeScript" is the prose). No
// file a language server can read declares it, so without this file
// every `veles.` in an extension is "Cannot find name 'veles'" and
// nothing completes. The New extension TypeScript scaffold copies it
// next to plugin.ts with a tsconfig.json that includes it; the Dev
// pane's Check and the contribution check pass it to tsc.
//
// A cargo test (extension_api_declaration_names_every_prelude_binding)
// fails when prelude.js gains a binding this file does not declare.

/** JSON as the engine answers it. */
type VelesJson = null | boolean | number | string | VelesJson[] | { [key: string]: VelesJson };

/** What a tool, command or hook handler may answer: a string, or any
 * value (JSON-encoded for the engine). */
type VelesAnswer = unknown;

interface VelesToolSpec {
  name: string;
  description?: string;
  /** JSON Schema of the arguments. */
  schema?: object;
}

interface VelesServiceSpec {
  name: string;
  description?: string;
  /** Who may call: omitted = core and in-process modules only, "any",
   * or an ACL capability. */
  cap?: string;
}

interface VelesServiceContext {
  caller: string;
  sessionId: string;
  /** What the layer below this override would answer. */
  next: (params: unknown) => Promise<unknown>;
}

interface VelesApi {
  /** Any engine operation by its Tcl name (`veles::<op>`). */
  call(op: string, ...args: unknown[]): Promise<string>;
  /** A line in the gateway journal under this plugin's owner. */
  log(text: unknown, level?: "debug" | "info" | "warn" | "error"): void;
  /** A model-callable tool. `handler(args, sessionId)`. */
  registerTool(
    spec: VelesToolSpec,
    handler: (args: any, sessionId: string) => VelesAnswer | Promise<VelesAnswer>,
  ): Promise<string>;
  /** A slash command: `handler(text)` renders the prompt text. */
  registerCommand(
    name: string,
    description: string | undefined,
    handler: (text: string) => VelesAnswer | Promise<VelesAnswer>,
  ): Promise<string>;
  /** A lifecycle hook: answer nothing ("no opinion") or a verdict. */
  hook(name: string, handler: (args: any) => VelesAnswer | Promise<VelesAnswer>): Promise<string>;
  /** A service other plugins call by name, or an override point. */
  registerService(
    spec: VelesServiceSpec,
    handler: (params: any, context: VelesServiceContext) => VelesAnswer | Promise<VelesAnswer>,
  ): Promise<string>;
  /** The layer below a provider/override, by the `next` token it got. */
  serviceNext(token: string, params: unknown): Promise<unknown>;
  /** Another plugin's service; a string answer stays a string. */
  serviceCall(name: string, params?: unknown, sessionId?: string): Promise<unknown>;
  /** Who provides and uses which service here. */
  services(): Promise<VelesJson>;
  /** Dispatch a tool, judged as `sessionId`'s principal when given. */
  toolCall(name: string, args?: unknown, sessionId?: string): Promise<string>;
  /** `system, user, provider?, model?` — the assistant text. */
  callLLM(system: string, user: string, provider?: string, model?: string): Promise<string>;
  httpGet(url: string, headersJson?: string): Promise<string>;
  httpPost(url: string, body: unknown, contentType?: string, headersJson?: string, timeoutSecs?: string): Promise<string>;
  /** Streaming transport for a provider driver: open, then next, then close. */
  httpStreamOpen(url: string, body: string, contentType?: string, headersJson?: string): Promise<string>;
  httpStreamNext(id: string | number): Promise<string>;
  httpStreamClose(id: string | number): Promise<string>;
  /** A dotted config path, as JSON text. */
  configGet(path: string): Promise<string>;
  /** Edit config.toml in place; "" or null unsets. */
  configSet(path: string, value: unknown): Promise<string>;
  note(title: string, body?: string): Promise<string>;
  sendOutbound(platform: string, chatId: string, text: string): Promise<string>;
  principal(sessionId?: string): Promise<string>;
  sandboxWhere(sessionId?: string): Promise<string>;
  sessions(): Promise<string>;
  history(sessionId: string, limit?: string | number): Promise<string>;
  usageTotals(sessionId?: string): Promise<string>;
  limits(): Promise<string>;
  nodes(): Promise<string>;
  routes(): Promise<string>;
  tasks(): Promise<string>;
  taskCancel(id: string): Promise<string>;
  /** A provider driver's chunk, as JSON text. */
  providerEmit(chunkJson: string): Promise<string>;
  /** One (or a chain of) keys out of a JSON document. */
  jsonGet(json: string, key: string, ...keys: string[]): Promise<string>;
}

declare const veles: VelesApi;

// The other globals the prelude installs. An extension's tsconfig has no
// DOM or Node library (neither exists in the host), so these are the only
// declarations of them.
declare const console: {
  log(...values: unknown[]): void;
  info(...values: unknown[]): void;
  debug(...values: unknown[]): void;
  warn(...values: unknown[]): void;
  error(...values: unknown[]): void;
};
declare function setTimeout(callback: (...args: any[]) => void, ms?: number, ...args: any[]): number;
declare function clearTimeout(id: number): void;
declare function setInterval(callback: (...args: any[]) => void, ms?: number, ...args: any[]): number;
declare function clearInterval(id: number): void;
