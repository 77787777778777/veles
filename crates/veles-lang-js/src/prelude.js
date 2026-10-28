// The veles API as a JavaScript plugin sees it (crates/veles-lang-js).
// Evaluated once per script context, before the script's entry. Three
// natives are bound by the host beneath it: `__veles_call(op, args)`
// (a Promise of the engine's answer), `__veles_log(level, text)` and
// `__veles_sleep(ms)` (a Promise). Everything here is sugar over those.
(() => {
  const callbacks = new Map();
  let nextCallback = 1;
  const register = (fn) => {
    const id = nextCallback++;
    callbacks.set(id, fn);
    return id;
  };
  const toText = (v) =>
    typeof v === "string" ? v : v === undefined || v === null ? "" : JSON.stringify(v);

  // One door into the engine: the ops are bridgeops' table plus the
  // four registrations (veles_core::scripthost::ScriptCx::call).
  async function call(op, ...args) {
    const answer = JSON.parse(await __veles_call(String(op), args.map(toText)));
    if (answer.err !== undefined) throw new Error(answer.err);
    return answer.ok;
  }

  // What the layer below a provider answers (PLAN-extension-api.md D2):
  // `token` is the `next` its own request carried, valid while it runs.
  const serviceNext = async (token, params) => {
    const out = await call("service_next", token ?? "", JSON.stringify(params ?? {}));
    try {
      return JSON.parse(out);
    } catch {
      return out;
    }
  };

  const veles = {
    call,
    log: (text, level = "info") => __veles_log(String(level), toText(text)),
    // A tool: `handler(args, sessionId)` answers a string or a value
    // (JSON-encoded for the model). The schema is JSON Schema.
    registerTool: (spec, handler) =>
      call(
        "register_tool",
        spec.name,
        spec.description ?? "",
        JSON.stringify(spec.schema ?? { type: "object", properties: {} }),
        register(async (p) => handler(p.args, p.session_id)),
      ),
    // A slash command: `handler(text)` renders the prompt text.
    registerCommand: (name, description, handler) =>
      call("register_command", name, description ?? "", register(async (p) => handler(p.args))),
    // A hook: `handler(args)` answers nothing ("no opinion") or a
    // verdict object/string; the first non-empty verdict wins.
    hook: (name, handler) => call("hook", name, register(async (p) => handler(p.args))),
    // A service other plugins call by name, or an override point:
    // `handler(params, {caller, sessionId, next})` answers its JSON;
    // `await next(params)` is what the layer below would answer. `cap` is
    // who may call: omitted = core and in-process modules only, "any", or
    // an ACL capability.
    registerService: (spec, handler) =>
      call(
        "register_service",
        spec.name,
        spec.description ?? "",
        spec.cap ?? "",
        register(async (p) =>
          handler(p.params, {
            caller: p.caller,
            sessionId: p.session_id,
            next: (params) => serviceNext(p.next, params),
          }),
        ),
      ),
    serviceNext,
    // Another plugin's service: its JSON answer (a string stays a string).
    serviceCall: async (name, params, sessionId) => {
      const args = [name, JSON.stringify(params ?? {})];
      if (sessionId !== undefined) args.push(sessionId);
      const out = await call("service_call", ...args);
      try {
        return JSON.parse(out);
      } catch {
        return out;
      }
    },
    // Who provides and uses which service here.
    services: async () => JSON.parse(await call("services")),
    toolCall: (name, args, sessionId) =>
      sessionId === undefined
        ? call("tool_call", name, JSON.stringify(args ?? {}))
        : call("tool_call", name, JSON.stringify(args ?? {}), sessionId),
    callLLM: (...a) => call("call_llm", ...a),
    httpGet: (url) => call("http_get", url),
    httpPost: (url, body) => call("http_post", url, toText(body)),
    // The streaming transport, for a provider driver on a wire that
    // streams. Three ops rather than one because a driver runs on a
    // blocking thread and has to PULL: open answers as soon as the
    // response status is known (so 429/5xx can be classified before any
    // body is read), next yields one event at a time, close frees the
    // handle. Variadic so the optional content-type and headers arguments
    // forward — a driver that cannot set headers cannot authenticate
    // against an endpoint that is not bearer-tokened.
    httpStreamOpen: (...a) => call("http_stream_open", ...a),
    httpStreamNext: (id) => call("http_stream_next", String(id)),
    httpStreamClose: (id) => call("http_stream_close", String(id)),
    configGet: (path) => call("config_get", path),
    configSet: (path, value) => call("config_set", path, toText(value)),
    note: (title, body) => call("note", title, body ?? ""),
    sendOutbound: (...a) => call("send_outbound", ...a),
    principal: (...a) => call("principal", ...a),
    sandboxWhere: (...a) => call("sandbox_where", ...a),
    sessions: () => call("sessions"),
    history: (...a) => call("history", ...a),
    usageTotals: (...a) => call("usage_totals", ...a),
    limits: () => call("limits"),
    nodes: () => call("nodes"),
    routes: () => call("routes"),
    tasks: () => call("tasks"),
    taskCancel: (id) => call("task_cancel", id),
    providerEmit: (...a) => call("provider_emit", ...a),
    jsonGet: (...a) => call("json_get", ...a),
  };
  Object.freeze(veles);
  Object.defineProperty(globalThis, "veles", { value: veles, writable: false, configurable: false });

  // The host's way back in: run callback `id` with a JSON payload and
  // answer JSON (a string answer stays a string; nothing is `null`).
  Object.defineProperty(globalThis, "__veles_invoke", {
    value: async (id, payloadJson) => {
      const fn = callbacks.get(id);
      if (fn === undefined) throw new Error(`no callback ${id} in this script`);
      const out = await fn(JSON.parse(payloadJson));
      return out === undefined ? "null" : JSON.stringify(out);
    },
    writable: false,
    configurable: false,
  });

  const line = (args) => args.map(toText).join(" ");
  globalThis.console = {
    log: (...a) => veles.log(line(a)),
    info: (...a) => veles.log(line(a)),
    debug: (...a) => veles.log(line(a), "debug"),
    warn: (...a) => veles.log(line(a), "warn"),
    error: (...a) => veles.log(line(a), "error"),
  };

  // Timers over the host's sleep; cleared ones simply never fire.
  const timers = new Set();
  let nextTimer = 1;
  globalThis.setTimeout = (fn, ms = 0, ...args) => {
    const id = nextTimer++;
    timers.add(id);
    __veles_sleep(Math.max(0, ms | 0)).then(() => {
      if (timers.delete(id)) fn(...args);
    });
    return id;
  };
  globalThis.clearTimeout = (id) => {
    timers.delete(id);
  };
  globalThis.setInterval = (fn, ms = 0, ...args) => {
    const id = nextTimer++;
    timers.add(id);
    const tick = () =>
      __veles_sleep(Math.max(1, ms | 0)).then(() => {
        if (timers.has(id)) {
          fn(...args);
          tick();
        }
      });
    tick();
    return id;
  };
  globalThis.clearInterval = globalThis.clearTimeout;
})();
