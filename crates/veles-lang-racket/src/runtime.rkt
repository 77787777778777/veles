#lang racket/base
;; veles/runtime — the lang-racket host's loop: the half of the host that
;; is written in Racket (crates/veles-lang-racket/src/runtime.rkt, copied
;; into the module's state dir as a collection when the engine boots).
;; ONE instance for the whole engine: the top-level namespace requires
;; it, and every script's namespace gets that instance attached, so the
;; tables below are shared.
;;
;; The protocol with the Rust side (engine.rs) is JSON text both ways.
;; Rust pushes a message into its mailbox and writes a byte to a pipe;
;; the main loop reads the byte and polls the mailbox dry (`poll`), and
;; answers by calling back into Rust (`send`). Handlers run as Racket
;; threads under their script's custodian, so a slow one blocks nothing,
;; one past its deadline is killed, and an unload is a custodian
;; shutdown. A host call a handler makes is a request with an id and an
;; async channel its reply lands in, so no Racket thread ever blocks the
;; one OS thread Racket runs on.
(require ffi/unsafe ffi/unsafe/port json racket/async-channel)
(provide veles-runtime-init! veles-main-loop
         current-script register-callback! host-call host-log)

;; --- the Rust side's three functions, bound from their addresses ---
(define poll-raw #f)
(define send-raw #f)
(define free-raw #f)
(define wake-port #f)
(define invoke-deadline 300)

(define (veles-runtime-init! poll-addr send-addr free-addr wake-fd deadline)
  (set! poll-raw (cast (cast poll-addr _intptr _pointer) _pointer (_fun -> _pointer)))
  (set! send-raw (cast (cast send-addr _intptr _pointer) _pointer (_fun _string/utf-8 -> _void)))
  (set! free-raw (cast (cast free-addr _intptr _pointer) _pointer (_fun _pointer -> _void)))
  (set! wake-port (unsafe-file-descriptor->port wake-fd 'veles-wake '(read)))
  (set! invoke-deadline deadline))

(define (poll-message)
  (define p (poll-raw))
  (and p
       (let ([s (cast p _pointer _string/utf-8)])
         (free-raw p)
         s)))

(define (send-json h) (send-raw (jsexpr->string h)))

;; --- the tables ---
(struct script (custodian namespace callbacks [next-callback #:mutable]))
(define scripts (make-hash))   ; name -> script
(define waiting (make-hash))   ; host-call id -> async channel
(define next-id 0)
(define (next-id!) (set! next-id (add1 next-id)) next-id)

(define current-script (make-parameter #f))

(define (no-exit code)
  (error 'exit "a plugin may not exit the gateway (exit ~a)" code))

(define (describe e)
  (if (exn? e) (exn-message e) (format "raised: ~e" e)))

;; A handler's answer as JSON text: (void) is "null" (no opinion), a
;; jsexpr is itself, anything else its printed form as a string.
(define (->json v)
  (cond [(void? v) "null"]
        [(jsexpr? v) (jsexpr->string v)]
        [else (jsexpr->string (format "~a" v))]))

(define (reply-ok id text) (send-json (hash 't "reply" 'id id 'ok text)))
(define (reply-err id text) (send-json (hash 't "reply" 'id id 'err text)))

;; --- what a script's API (veles/main.rkt) calls ---
(define (register-callback! proc)
  (define name (current-script))
  (define s (and name (hash-ref scripts name #f)))
  (unless s (error 'veles "no script is loading or running on this thread"))
  (define id (add1 (script-next-callback s)))
  (set-script-next-callback! s id)
  (hash-set! (script-callbacks s) id proc)
  (number->string id))

(define (host-call op args)
  (define id (next-id!))
  (define ch (make-async-channel))
  (hash-set! waiting id ch)
  (send-json (hash 't "host" 'id id 'name (or (current-script) "") 'op op 'args args))
  (define ans (async-channel-get ch))
  (if (hash-has-key? ans 'err)
      (error 'veles "~a" (hash-ref ans 'err))
      (hash-ref ans 'ok)))

(define (host-log level text)
  (send-json (hash 't "log" 'name (or (current-script) "") 'level level 'text text)))

;; --- the requests ---
(define (do-load m)
  (define id (hash-ref m 'id))
  (define name (hash-ref m 'name))
  (define entry (hash-ref m 'entry))
  (define dir (hash-ref m 'dir))
  ;; A reload: the earlier load's threads and its namespace go first.
  (define earlier (hash-ref scripts name #f))
  (when earlier
    (custodian-shutdown-all (script-custodian earlier))
    (hash-remove! scripts name))
  (define cust (make-custodian))
  (custodian-limit-memory cust (* 64 1024 1024))
  (define ns (make-base-empty-namespace))
  (namespace-attach-module (current-namespace) 'veles/runtime ns)
  (define s (script cust ns (make-hash) 0))
  (hash-set! scripts name s)
  (define t0 (current-inexact-milliseconds))
  (define outcome
    (with-handlers ([(lambda (e) #t) (lambda (e) (cons 'err (describe e)))])
      (parameterize ([current-namespace ns]
                     [current-custodian cust]
                     [current-script name]
                     [exit-handler no-exit]
                     [current-directory dir])
        (dynamic-require (string->path entry) #f))
      (cons 'ok (format "~a callback(s), compiled in ~a ms"
                        (hash-count (script-callbacks s))
                        (inexact->exact (round (- (current-inexact-milliseconds) t0)))))))
  (cond [(eq? (car outcome) 'ok) (reply-ok id (cdr outcome))]
        [else
         ;; A load that raised leaves nothing behind: its threads die
         ;; with the custodian; the seam purges its registrations.
         (custodian-shutdown-all cust)
         (hash-remove! scripts name)
         (reply-err id (cdr outcome))]))

(define (do-unload m)
  (define id (hash-ref m 'id))
  (define name (hash-ref m 'name))
  (define s (hash-ref scripts name #f))
  (cond [s (custodian-shutdown-all (script-custodian s))
           (hash-remove! scripts name)
           (reply-ok id (format "custodian shut down, ~a callback(s) dropped"
                                (hash-count (script-callbacks s))))]
        [else (reply-ok id "no namespace")]))

(define (do-invoke m)
  (define id (hash-ref m 'id))
  (define name (hash-ref m 'name))
  (define cb (hash-ref m 'cb))
  (define payload (hash-ref m 'payload))
  (define s (hash-ref scripts name #f))
  (define handler (and s (hash-ref (script-callbacks s) cb #f)))
  (cond
    [(not s) (reply-err id (format "racket:~a: not loaded" name))]
    [(not handler) (reply-err id (format "no callback ~a in racket:~a" cb name))]
    [else
     (define t
       (parameterize ([current-custodian (script-custodian s)]
                      [current-namespace (script-namespace s)]
                      [current-script name]
                      [exit-handler no-exit])
         (thread
          (lambda ()
            (define outcome
              (with-handlers ([(lambda (e) #t) (lambda (e) (cons 'err (describe e)))])
                (cons 'ok (->json (handler (string->jsexpr payload))))))
            (if (eq? (car outcome) 'ok)
                (reply-ok id (cdr outcome))
                (reply-err id (cdr outcome)))))))
     ;; The deadline: a handler still running when it passes is killed
     ;; and its caller told so (the seam's own deadline is what a caller
     ;; sees first; this keeps the thread from living on).
     (thread
      (lambda ()
        (unless (sync/timeout invoke-deadline t)
          (kill-thread t)
          (reply-err id (format "racket:~a: the handler ran ~a s without answering; killed"
                                name invoke-deadline)))))]))

(define (dispatch! m)
  (define t (hash-ref m 't ""))
  (cond
    [(equal? t "load") (thread (lambda () (do-load m)))]
    [(equal? t "unload") (do-unload m)]
    [(equal? t "invoke") (do-invoke m)]
    [(equal? t "host-reply")
     (define id (hash-ref m 'id))
     (define ch (hash-ref waiting id #f))
     (hash-remove! waiting id)
     (when ch (async-channel-put ch m))]
    [else (host-log "warn" (format "the loop: unknown message `~a`" t))]))

(define drain-buf (make-bytes 256))

(define (drain!)
  (let loop ()
    (define m (poll-message))
    (when m
      (with-handlers ([(lambda (e) #t)
                       (lambda (e) (host-log "error" (format "the loop: ~a" (describe e))))])
        (dispatch! (string->jsexpr m)))
      (loop))))

;; The loop: never returns. One byte on the pipe means "the mailbox has
;; something"; reading it blocks only this Racket thread, so handlers
;; keep running meanwhile.
(define (veles-main-loop)
  (send-json (hash 't "booted" 'version (version)))
  (let loop ()
    (with-handlers ([(lambda (e) #t)
                     (lambda (e) (host-log "error" (format "the loop: ~a" (describe e))))])
      (read-byte wake-port)
      (read-bytes-avail!* drain-buf wake-port)
      (drain!))
    (loop)))
