#lang racket/base
;; veles — the API a Racket plugin requires (`(require veles)`; this is
;; crates/veles-lang-racket/src/veles.rkt, copied into the module's state
;; dir as the collection's main.rkt when the engine boots). Every call
;; goes through the host's one door, `veles-call`, whose ops are
;; veles_core::bridgeops' table plus the four registrations: the same
;; operations Tcl's `veles::*` and JavaScript's `veles.*` have.
(require veles/runtime json)
(provide veles-call veles-log
         register-tool register-command hook
         register-service service-call service-next services
         tool-call call-llm http-get http-post
         http-stream-open http-stream-next http-stream-close
         config-get config-set note
         send-outbound principal sandbox-where sessions history usage-totals
         limits nodes routes tasks task-cancel provider-emit json-get)

;; An argument as the text the engine takes: a string as it is, (void)
;; as nothing, a jsexpr as JSON, anything else printed.
(define (->text v)
  (cond [(string? v) v]
        [(void? v) ""]
        [(jsexpr? v) (jsexpr->string v)]
        [else (format "~a" v)]))

(define (veles-call op . args) (host-call op (map ->text args)))
(define (veles-log text [level "info"]) (host-log level (->text text)))

;; A tool: `(handler args session-id)` answers a string or a jsexpr
;; (JSON for the model). The schema is JSON Schema as a jsexpr.
(define (register-tool name description schema handler)
  (veles-call "register_tool" name description (jsexpr->string schema)
              (register-callback!
               (lambda (p) (handler (hash-ref p 'args (hash)) (hash-ref p 'session_id ""))))))
;; A slash command: `(handler text)` renders the prompt text.
(define (register-command name description handler)
  (veles-call "register_command" name description
              (register-callback! (lambda (p) (handler (hash-ref p 'args ""))))))
;; A hook: `(handler args)` answers (void) for "no opinion", or a
;; verdict (a hash or a string); the first non-empty verdict wins.
(define (hook name handler)
  (veles-call "hook" name
              (register-callback! (lambda (p) (handler (hash-ref p 'args (hash)))))))
;; A service other plugins call by name, or an override point: `(handler
;; params caller session-id)` answers a string or a jsexpr, and inside it
;; `(service-next params)` is what the layer below would answer. `#:cap`
;; is who may call: omitted = core and in-process modules only, "any", or
;; an ACL capability.
(define (register-service name description handler #:cap [cap ""])
  (veles-call "register_service" name description cap
              (register-callback!
               (lambda (p)
                 (define t (hash-ref p 'next ""))
                 (parameterize ([current-next-token (if (string? t) t "")])
                   (handler (hash-ref p 'params (hash))
                            (hash-ref p 'caller "")
                            (hash-ref p 'session_id "")))))))
;; The `next` token of the service request a handler is answering
;; (PLAN-extension-api.md D2), valid while that request runs.
(define current-next-token (make-parameter ""))
;; What the layer below a provider answers to `params`: an override
;; point's provider (or any service's) wrapping what it covers. Call it
;; from inside the handler; the answer is a jsexpr, or text.
(define (service-next params [token (current-next-token)])
  (define out (veles-call "service_next" token (jsexpr->string params)))
  (with-handlers ([exn:fail:read? (lambda (e) out)]) (string->jsexpr out)))
;; Another plugin's service: its JSON answer as a jsexpr (an answer that
;; is not JSON stays the text it is).
(define (service-call name params [session-id #f])
  (define out
    (if session-id
        (veles-call "service_call" name (jsexpr->string params) session-id)
        (veles-call "service_call" name (jsexpr->string params))))
  (with-handlers ([exn:fail:read? (lambda (e) out)]) (string->jsexpr out)))
;; The services on offer, as the registry lists them (a list of hashes).
(define (services) (string->jsexpr (veles-call "services")))

(define (tool-call name args [session-id #f])
  (if session-id
      (veles-call "tool_call" name (jsexpr->string args) session-id)
      (veles-call "tool_call" name (jsexpr->string args))))
(define (call-llm . a) (apply veles-call "call_llm" a))
(define (http-get url) (veles-call "http_get" url))
(define (http-post url body) (veles-call "http_post" url body))
;; The streaming transport, for a provider driver on a wire that streams.
;; Three ops rather than one because a driver runs on a blocking thread and
;; has to PULL: open answers as soon as the response status is known (so
;; 429/5xx can be classified before any body is read), next yields one event
;; at a time, close frees the handle. Optional content-type and headers are
;; variadic so a driver can authenticate against an endpoint that is not
;; bearer-tokened.
(define (http-stream-open url body . a) (apply veles-call "http_stream_open" url body a))
(define (http-stream-next id) (veles-call "http_stream_next" (format "~a" id)))
(define (http-stream-close id) (veles-call "http_stream_close" (format "~a" id)))
(define (config-get path) (veles-call "config_get" path))
(define (config-set path value) (veles-call "config_set" path value))
(define (note title [body ""]) (veles-call "note" title body))
(define (send-outbound . a) (apply veles-call "send_outbound" a))
(define (principal . a) (apply veles-call "principal" a))
(define (sandbox-where . a) (apply veles-call "sandbox_where" a))
(define (sessions) (veles-call "sessions"))
(define (history . a) (apply veles-call "history" a))
(define (usage-totals . a) (apply veles-call "usage_totals" a))
(define (limits) (veles-call "limits"))
(define (nodes) (veles-call "nodes"))
(define (routes) (veles-call "routes"))
(define (tasks) (veles-call "tasks"))
(define (task-cancel id) (veles-call "task_cancel" id))
(define (provider-emit . a) (apply veles-call "provider_emit" a))
(define (json-get . a) (apply veles-call "json_get" a))
