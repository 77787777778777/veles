;;;; veles — the API a Common Lisp plugin uses (crates/veles-lang-lisp/
;;;; src/veles.lisp, loaded once when the engine boots). A plugin's file is
;;;; loaded into a package of its own that uses COMMON-LISP and VELES.
;;;; Every call goes through the host's one door, VELES:CALL, whose ops
;;;; are veles_core::bridgeops' table plus the four registrations: the
;;;; same operations Tcl's `veles::*`, JavaScript's `veles.*` and Racket's
;;;; `veles` have.
;;;;
;;;; JSON comes in as EQUAL hash tables with string keys and vectors;
;;;; true is T, false is NIL, null is :NULL. A handler answers a string or
;;;; anything JSON-STRING writes (a hash table, a vector or list, a number,
;;;; T, :FALSE); NIL is null, which a hook means as "no opinion".

(defpackage #:veles
  (:use #:cl)
  (:import-from #:veles-runtime
                #:json-parse #:json-string #:obj #:spawn)
  (:export #:call #:log-line
           #:register-tool #:register-command #:hook
           #:register-service #:service-call #:service-next #:services
           #:tool-call #:call-llm #:http-get #:http-post
           #:http-stream-open #:http-stream-next #:http-stream-close
           #:config-get #:config-set
           #:note #:send-outbound #:principal #:sandbox-where #:sessions #:history
           #:usage-totals #:limits #:nodes #:routes #:tasks #:task-cancel
           #:provider-emit #:json-get
           #:json-parse #:json-string #:obj #:spawn))

(in-package #:veles)

(defun ->text (v)
  "An argument as the text the engine takes: a string as it is, NIL as
nothing, anything JSON can say as JSON."
  (cond ((stringp v) v)
        ((null v) "")
        ((or (hash-table-p v) (vectorp v) (consp v) (numberp v) (eq v t) (eq v :false) (eq v :null))
         (json-string v))
        (t (princ-to-string v))))

(defun call (op &rest args)
  (veles-runtime:host-call op (mapcar #'->text args)))

(defun log-line (text &optional (level "info"))
  (veles-runtime:host-log level (->text text)))

(defun empty-object () (make-hash-table :test 'equal))

(defun register-tool (name description schema handler)
  "A tool: (HANDLER args session-id) answers a string or JSON for the
model. SCHEMA is JSON Schema, as JSON text or a hash table."
  (call "register_tool" name description (->text schema)
        (veles-runtime:register-callback
         (lambda (p)
           (funcall handler (gethash "args" p (empty-object)) (gethash "session_id" p ""))))))

(defun register-command (name description handler)
  "A slash command: (HANDLER text) renders the prompt text."
  (call "register_command" name description
        (veles-runtime:register-callback
         (lambda (p) (funcall handler (gethash "args" p ""))))))

(defun hook (name handler)
  "A hook: (HANDLER args) answers NIL for \"no opinion\", or a verdict (a
hash table or a string); the first non-empty verdict wins."
  (call "hook" name
        (veles-runtime:register-callback
         (lambda (p) (funcall handler (gethash "args" p (empty-object)))))))

(defvar *next-token* ""
  "The `next` token of the service request a handler is answering
(PLAN-extension-api.md D2), bound around the handler's call.")

(defun register-service (name description handler &key (cap ""))
  "A service other plugins call by name, or an override point: (HANDLER
params caller session-id) answers a string or JSON, and inside it
(SERVICE-NEXT params) is what the layer below would answer. CAP is who
may call: \"\" = core and in-process modules only, \"any\", or an ACL
capability."
  (call "register_service" name description cap
        (veles-runtime:register-callback
         (lambda (p)
           (let ((*next-token* (let ((tok (gethash "next" p "")))
                                 (if (stringp tok) tok ""))))
             (funcall handler (gethash "params" p (empty-object))
                      (gethash "caller" p "")
                      (gethash "session_id" p "")))))))

(defun service-next (params &optional (token *next-token*))
  "What the layer below a provider answers to PARAMS: an override point's
provider (or any service's) wrapping what it covers. Call it from inside
the handler; the answer is read JSON, or text."
  (let ((out (call "service_next" token (json-string params))))
    (handler-case (json-parse out)
      (error () out))))

(defun service-call (name params &optional session-id)
  "Another plugin's service: its JSON answer, read (an answer that is not
JSON stays the text it is)."
  (let ((out (if session-id
                 (call "service_call" name (json-string params) session-id)
                 (call "service_call" name (json-string params)))))
    (handler-case (json-parse out)
      (error () out))))

(defun services ()
  "The services on offer, as the registry lists them (a vector of hash tables)."
  (json-parse (call "services")))

(defun tool-call (name args &optional session-id)
  (if session-id
      (call "tool_call" name (json-string args) session-id)
      (call "tool_call" name (json-string args))))
(defun call-llm (&rest a) (apply #'call "call_llm" a))
(defun http-get (url) (call "http_get" url))
(defun http-post (url body) (call "http_post" url body))
;; The streaming transport, for a provider driver on a wire that streams.
;; Three ops rather than one because a driver runs on a blocking thread and
;; has to PULL: open answers as soon as the response status is known (so
;; 429/5xx can be classified before any body is read), next yields one event
;; at a time, close frees the handle. Optional content-type and headers are
;; &rest so a driver can authenticate against an endpoint that is not
;; bearer-tokened.
(defun http-stream-open (url body &rest a) (apply #'call "http_stream_open" url body a))
(defun http-stream-next (id) (call "http_stream_next" (princ-to-string id)))
(defun http-stream-close (id) (call "http_stream_close" (princ-to-string id)))
(defun config-get (path) (call "config_get" path))
(defun config-set (path value) (call "config_set" path value))
(defun note (title &optional (body "")) (call "note" title body))
(defun send-outbound (&rest a) (apply #'call "send_outbound" a))
(defun principal (&rest a) (apply #'call "principal" a))
(defun sandbox-where (&rest a) (apply #'call "sandbox_where" a))
(defun sessions () (call "sessions"))
(defun history (&rest a) (apply #'call "history" a))
(defun usage-totals (&rest a) (apply #'call "usage_totals" a))
(defun limits () (call "limits"))
(defun nodes () (call "nodes"))
(defun routes () (call "routes"))
(defun tasks () (call "tasks"))
(defun task-cancel (id) (call "task_cancel" id))
(defun provider-emit (&rest a) (apply #'call "provider_emit" a))
(defun json-get (&rest a) (apply #'call "json_get" a))
