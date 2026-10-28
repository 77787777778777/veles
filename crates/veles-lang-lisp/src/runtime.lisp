;;;; veles-runtime — the lang-lisp host's loop: the half of the host that
;;;; is written in Lisp (crates/veles-lang-lisp/src/runtime.lisp, written
;;;; into the module's state dir and loaded once when the engine boots).
;;;;
;;;; The protocol with the Rust side (engine.rs) is JSON text both ways,
;;;; and ASCII on the wire: every character past 127 travels as a \u
;;;; escape, so a string crosses as a base string with no encoding step.
;;;; Rust keeps a mailbox; the loop takes the next message with
;;;; VELES-SYS:%NEXT, which blocks this OS thread (ECL's threads are real
;;;; threads) for at most the given milliseconds, and answers with
;;;; VELES-SYS:%SEND. Loads and handlers run in threads of their own, so a
;;;; slow one blocks nothing; a handler past its deadline is killed; an
;;;; unload kills the script's threads and deletes its packages. A host
;;;; call a handler makes is a request with an id and a cell its reply
;;;; lands in.

(defpackage #:veles-runtime
  (:use #:cl)
  (:export #:main-loop #:host-call #:host-log #:register-callback #:spawn
           #:*script* #:json-parse #:json-string #:obj))

(in-package #:veles-runtime)

;;; --- JSON (objects are EQUAL hash tables with string keys, arrays are
;;; vectors; true is T, false is NIL, null is :NULL; on the way out NIL
;;; and :NULL are null, :FALSE is false, a list is an array) ---

(defun json-write-string (s out)
  (write-char #\" out)
  (loop for c across s
        for code = (char-code c)
        do (cond ((char= c #\") (write-string "\\\"" out))
                 ((char= c #\\) (write-string "\\\\" out))
                 ((< code 32) (format out "\\u~4,'0x" code))
                 ((< code 128) (write-char c out))
                 ((< code #x10000) (format out "\\u~4,'0x" code))
                 (t (let ((v (- code #x10000)))
                      (format out "\\u~4,'0x\\u~4,'0x"
                              (+ #xD800 (ash v -10))
                              (+ #xDC00 (logand v #x3FF)))))))
  (write-char #\" out))

(defun json-write-float (x out)
  (let ((d (coerce x 'double-float)))
    (if (float-nan-or-inf-p d)
        (write-string "null" out)
        (let ((*read-default-float-format* 'double-float))
          (write-string (prin1-to-string d) out)))))

(defun float-nan-or-inf-p (d)
  ;; NaN is not = to itself; an infinity is past the largest finite.
  (or (/= d d) (> (abs d) most-positive-double-float)))

(defun json-key (k)
  (cond ((stringp k) k)
        ((symbolp k) (string-downcase (symbol-name k)))
        (t (princ-to-string k))))

(defun json-write (v out)
  (cond ((eq v t) (write-string "true" out))
        ((or (null v) (eq v :null)) (write-string "null" out))
        ((eq v :false) (write-string "false" out))
        ((stringp v) (json-write-string v out))
        ((integerp v) (format out "~d" v))
        ((realp v) (json-write-float v out))
        ((hash-table-p v)
         (write-char #\{ out)
         (let ((first t))
           (maphash (lambda (k val)
                      (unless first (write-char #\, out))
                      (setf first nil)
                      (json-write-string (json-key k) out)
                      (write-char #\: out)
                      (json-write val out))
                    v))
         (write-char #\} out))
        ((or (vectorp v) (consp v))
         (write-char #\[ out)
         (let ((first t))
           (map nil (lambda (e)
                      (unless first (write-char #\, out))
                      (setf first nil)
                      (json-write e out))
                v))
         (write-char #\] out))
        ((symbolp v) (json-write-string (string-downcase (symbol-name v)) out))
        (t (json-write-string (princ-to-string v) out))))

(defun json-string (v)
  "V as JSON text (ASCII: anything past 127 is a \\u escape)."
  (with-output-to-string (out) (json-write v out)))

(defun obj (&rest plist)
  "A JSON object from alternating keys and values: (obj \"a\" 1 \"b\" 2)."
  (let ((h (make-hash-table :test 'equal)))
    (loop for (k v) on plist by #'cddr do (setf (gethash (json-key k) h) v))
    h))

(define-condition json-error (error)
  ((text :initarg :text :reader json-error-text))
  (:report (lambda (c s) (format s "bad JSON: ~a" (json-error-text c)))))

(defun json-parse (text)
  "TEXT read as JSON: objects as EQUAL hash tables, arrays as vectors,
true T, false NIL, null :NULL, numbers as integers or double floats."
  (let ((pos 0) (len (length text)))
    (labels ((fail (why) (error 'json-error :text (format nil "~a at ~a" why pos)))
             (peek () (if (< pos len) (char text pos) nil))
             (next () (if (< pos len) (prog1 (char text pos) (incf pos)) (fail "the end")))
             (ws () (loop while (and (< pos len) (member (char text pos) '(#\Space #\Tab #\Newline #\Return)))
                          do (incf pos)))
             (expect (c) (unless (eql (next) c) (fail (format nil "expected ~a" c))))
             (word (w v) (if (and (<= (+ pos (length w)) len) (string= w text :start2 pos :end2 (+ pos (length w))))
                             (progn (incf pos (length w)) v)
                             (fail "a bad literal")))
             (hex4 () (if (<= (+ pos 4) len)
                          (prog1 (or (parse-integer text :start pos :end (+ pos 4) :radix 16 :junk-allowed nil)
                                     (fail "a bad \\u"))
                            (incf pos 4))
                          (fail "a short \\u")))
             (str ()
               (expect #\")
               (with-output-to-string (out)
                 (loop
                   (let ((c (next)))
                     (cond ((char= c #\") (return))
                           ((char= c #\\)
                            (let ((e (next)))
                              (case e
                                (#\" (write-char #\" out)) (#\\ (write-char #\\ out)) (#\/ (write-char #\/ out))
                                (#\b (write-char (code-char 8) out)) (#\f (write-char (code-char 12) out))
                                (#\n (write-char #\Newline out)) (#\r (write-char #\Return out))
                                (#\t (write-char #\Tab out))
                                (#\u (let ((u (hex4)))
                                       (if (and (<= #xD800 u #xDBFF) (< (+ pos 1) len)
                                                (char= (char text pos) #\\) (char= (char text (1+ pos)) #\u))
                                           (progn (incf pos 2)
                                                  (let ((lo (hex4)))
                                                    (write-char (code-char (+ #x10000 (ash (- u #xD800) 10) (- lo #xDC00))) out)))
                                           (write-char (code-char u) out))))
                                (t (fail "a bad escape")))))
                           (t (write-char c out)))))))
             (num ()
               (let ((start pos))
                 (loop while (and (< pos len) (find (char text pos) "+-0123456789.eE")) do (incf pos))
                 (let ((tok (subseq text start pos)))
                   (if (every (lambda (c) (find c "+-0123456789")) tok)
                       (or (parse-integer tok :junk-allowed nil) (fail "a bad number"))
                       (let ((*read-default-float-format* 'double-float)
                             (*read-eval* nil))
                         (let ((v (ignore-errors (read-from-string tok))))
                           (if (realp v) (coerce v 'double-float) (fail "a bad number"))))))))
             (val ()
               (ws)
               (let ((c (peek)))
                 (cond ((null c) (fail "the end"))
                       ((char= c #\{)
                        (next) (ws)
                        (let ((h (make-hash-table :test 'equal)))
                          (if (eql (peek) #\})
                              (next)
                              (loop (ws)
                                    (let ((k (str))) (ws) (expect #\:) (setf (gethash k h) (val)))
                                    (ws)
                                    (let ((d (next)))
                                      (cond ((char= d #\,))
                                            ((char= d #\}) (return))
                                            (t (fail "expected , or }"))))))
                          h))
                       ((char= c #\[)
                        (next) (ws)
                        (let ((items '()))
                          (if (eql (peek) #\])
                              (next)
                              (loop (push (val) items)
                                    (ws)
                                    (let ((d (next)))
                                      (cond ((char= d #\,))
                                            ((char= d #\]) (return))
                                            (t (fail "expected , or ]"))))))
                          (coerce (nreverse items) 'simple-vector)))
                       ((char= c #\") (str))
                       ((char= c #\t) (word "true" t))
                       ((char= c #\f) (word "false" nil))
                       ((char= c #\n) (word "null" :null))
                       (t (num))))))
      (prog1 (val)
        (ws)
        (when (< pos len) (fail "trailing text"))))))

;;; --- the wire ---

(defun send-json (v)
  ;; Interrupts wait while this thread is in Rust: a kill that landed
  ;; inside the native call would unwind through Rust frames.
  (let ((text (coerce (json-string v) 'simple-base-string)))
    (mp:without-interrupts (veles-sys:%send text))))

(defun host-log (level text)
  (send-json (obj "t" "log" "name" (or *script* "") "level" level "text" text)))

(defun reply-ok (id text) (send-json (obj "t" "reply" "id" id "ok" text)))
(defun reply-err (id text) (send-json (obj "t" "reply" "id" id "err" text)))

(defun describe-condition (c)
  (or (ignore-errors (princ-to-string c))
      (format nil "a ~(~a~)" (type-of c))))

;;; --- the tables ---

(defvar *script* nil "The name of the script this thread runs for.")

(defstruct script
  name
  (packages '())
  (callbacks (make-hash-table :synchronized t))
  (next-callback 0)
  (threads '())
  (lock (mp:make-lock :name "veles-script")))

(defvar *scripts* (make-hash-table :test 'equal :synchronized t))
(defvar *waiting* (make-hash-table :synchronized t))
(defvar *ids-lock* (mp:make-lock :name "veles-ids"))
(defvar *next-id* 0)
(defvar *answer-lock* (mp:make-lock :name "veles-answer"))
(defvar *invoke-deadline* 300)
;; (deadline-time process id name flag) per running handler; the loop's own.
(defvar *deadlines* '())

(defun next-id () (mp:with-lock (*ids-lock*) (incf *next-id*)))

(defun now-ms () (floor (* 1000 (get-internal-real-time)) internal-time-units-per-second))

(defun claim (flag)
  "T for the first caller: the one that answers a request."
  (mp:with-lock (*answer-lock*)
    (unless (car flag) (setf (car flag) t))))

(defun run-thread (s name fn)
  "A thread of script S, recorded under the script's lock before it can
end: its own DROP-THREAD waits for that lock, so a handler that answers at
once never leaves a dead entry behind."
  (mp:with-lock ((script-lock s))
    (let ((p (mp:process-run-function name fn)))
      (push p (script-threads s))
      p)))

(defun add-thread (s p)
  (mp:with-lock ((script-lock s)) (push p (script-threads s))))

(defun drop-thread (s p)
  (mp:with-lock ((script-lock s)) (setf (script-threads s) (remove p (script-threads s)))))

(defun current-script ()
  (and *script* (gethash *script* *scripts*)))

(defun spawn (name fn)
  "A thread that belongs to this script: it dies with the script's unload."
  (let* ((s (or (current-script) (error "veles: no script is loading or running on this thread")))
         (script-name *script*)
         (pkg *package*))
    (run-thread s (format nil "lisp:~a:~a" script-name name)
                (lambda ()
                  (let ((*script* script-name) (*package* pkg))
                    (unwind-protect (funcall fn)
                      (drop-thread s mp:*current-process*)))))))

(defun drop-script (s)
  "Kill the script's threads (but this one) and delete its packages."
  (let ((threads (mp:with-lock ((script-lock s)) (copy-list (script-threads s)))))
    (dolist (p threads)
      (unless (eq p mp:*current-process*)
        (ignore-errors (mp:process-kill p))))
    (dolist (pkg (script-packages s))
      (ignore-errors (delete-package pkg)))
    (clrhash (script-callbacks s))
    (length threads)))

;;; --- what a script's API (veles.lisp) calls ---

(defun register-callback (fn)
  (let ((s (or (current-script) (error "veles: no script is loading or running on this thread"))))
    (mp:with-lock ((script-lock s))
      (let ((id (incf (script-next-callback s))))
        (setf (gethash id (script-callbacks s)) fn)
        (princ-to-string id)))))

(defstruct cell
  (lock (mp:make-lock :name "veles-cell"))
  (cv (mp:make-condition-variable))
  (done nil)
  (value nil))

(defun cell-put (c v)
  (mp:with-lock ((cell-lock c))
    (setf (cell-value c) v (cell-done c) t)
    (mp:condition-variable-broadcast (cell-cv c))))

(defun cell-wait (c)
  (mp:with-lock ((cell-lock c))
    (loop until (cell-done c)
          do (mp:condition-variable-wait (cell-cv c) (cell-lock c)))
    (cell-value c)))

(defun host-call (op args)
  (let ((id (next-id))
        (c (make-cell)))
    (setf (gethash id *waiting*) c)
    (send-json (obj "t" "host" "id" id "name" (or *script* "") "op" op
                    "args" (coerce args 'simple-vector)))
    (let ((ans (cell-wait c)))
      (multiple-value-bind (err has-err) (gethash "err" ans)
        (if has-err
            (error "~a" err)
            (values (gethash "ok" ans "")))))))

;;; --- the requests ---

(defun no-exit (&rest args)
  (error "a plugin may not exit the gateway (exit~{ ~a~})" args))

(defun package-name-for (name)
  (format nil "VELES-SCRIPT/~:@(~a~)" name))

(defun do-load (m)
  (let* ((id (gethash "id" m))
         (name (gethash "name" m))
         (entry (gethash "entry" m))
         (dir (gethash "dir" m))
         (earlier (gethash name *scripts*)))
    ;; A reload: the earlier load's threads and packages go first.
    (when earlier
      (drop-script earlier)
      (remhash name *scripts*))
    (let* ((s (make-script :name name))
           (before (list-all-packages))
           (old (find-package (package-name-for name)))
           (t0 (now-ms)))
      (when old (ignore-errors (delete-package old)))
      (let ((pkg (make-package (package-name-for name) :use '("COMMON-LISP" "VELES"))))
        (push pkg (script-packages s))
        (setf (gethash name *scripts*) s)
        (add-thread s mp:*current-process*)
        (let ((outcome
                (handler-case
                    (let ((*script* name)
                          (*package* pkg)
                          (*default-pathname-defaults*
                            (pathname (if (and (plusp (length dir)) (char= (char dir (1- (length dir))) #\/))
                                          dir
                                          (concatenate 'string dir "/"))))
                          (*load-verbose* nil) (*load-print* nil)
                          (*compile-verbose* nil) (*compile-print* nil))
                      (handler-bind ((warning #'muffle-warning))
                        (load entry))
                      (cons :ok (format nil "~a callback(s), evaluated in ~a ms"
                                        (hash-table-count (script-callbacks s))
                                        (- (now-ms) t0))))
                  (serious-condition (e) (cons :err (describe-condition e))))))
          (drop-thread s mp:*current-process*)
          ;; The packages the load made are the script's too.
          (dolist (p (set-difference (list-all-packages) before))
            (pushnew p (script-packages s)))
          (cond ((eq (car outcome) :ok) (reply-ok id (cdr outcome)))
                (t
                 ;; A load that raised leaves nothing behind: its threads
                 ;; die, its packages go; the seam purges its registrations.
                 (drop-script s)
                 (remhash name *scripts*)
                 (reply-err id (cdr outcome)))))))))

(defun do-unload (m)
  (let* ((id (gethash "id" m))
         (name (gethash "name" m))
         (s (gethash name *scripts*)))
    (cond (s (let ((n (hash-table-count (script-callbacks s)))
                   (killed (drop-script s)))
               (remhash name *scripts*)
               (reply-ok id (format nil "package deleted, ~a thread(s) killed, ~a callback(s) dropped"
                                    killed n))))
          (t (reply-ok id "no package")))))

(defun do-invoke (m)
  (let* ((id (gethash "id" m))
         (name (gethash "name" m))
         (cb (gethash "cb" m))
         (payload (gethash "payload" m))
         (s (gethash name *scripts*))
         (handler (and s (gethash cb (script-callbacks s)))))
    (cond
      ((not s) (reply-err id (format nil "lisp:~a: not loaded" name)))
      ((not handler) (reply-err id (format nil "no callback ~a in lisp:~a" cb name)))
      (t
       (let* ((flag (list nil))
              (pkg (first (last (script-packages s))))
              (p (run-thread
                  s (format nil "lisp:~a#~a" name cb)
                  (lambda ()
                    (let ((*script* name) (*package* (or pkg *package*)))
                      (unwind-protect
                           (let ((outcome
                                   (handler-case
                                       (cons :ok (json-string (funcall handler (json-parse payload))))
                                     (serious-condition (e) (cons :err (describe-condition e))))))
                             (when (claim flag)
                               (if (eq (car outcome) :ok)
                                   (reply-ok id (cdr outcome))
                                   (reply-err id (cdr outcome)))))
                        (drop-thread s mp:*current-process*)))))))
         (push (list (+ (now-ms) (* 1000 *invoke-deadline*)) p id name flag) *deadlines*))))))

(defun check-deadlines ()
  ;; The deadline: a handler still running when it passes is killed and
  ;; its caller told so (the seam's own deadline is what a caller sees
  ;; first; this keeps the thread from living on).
  (let ((now (now-ms)) (keep '()))
    (dolist (d *deadlines*)
      (destructuring-bind (at p id name flag) d
        (cond ((or (car flag) (not (mp:process-active-p p))))
              ((>= now at)
               (when (claim flag)
                 (ignore-errors (mp:process-kill p))
                 (reply-err id (format nil "lisp:~a: the handler ran ~a s without answering; killed"
                                       name *invoke-deadline*))))
              (t (push d keep)))))
    (setf *deadlines* keep)))

(defun dispatch (m)
  (let ((tag (gethash "t" m "")))
    (cond
      ((equal tag "load")
       (mp:process-run-function (format nil "lisp-load:~a" (gethash "name" m)) (lambda () (do-load m))))
      ((equal tag "unload") (do-unload m))
      ((equal tag "invoke") (do-invoke m))
      ((equal tag "host-reply")
       (let* ((id (gethash "id" m))
              (c (gethash id *waiting*)))
         (remhash id *waiting*)
         (when c (cell-put c m))))
      (t (host-log "warn" (format nil "the loop: unknown message `~a`" tag))))))

;;; --- the process-wide guards, then the loop ---

(defun unhandled (condition hook)
  "An error nothing handled, in a thread a script made: logged, and the
thread ends. ECL's own debugger would read the gateway's stdin."
  (declare (ignore hook))
  (ignore-errors
   (host-log "error" (format nil "unhandled in thread ~a: ~a"
                             (ignore-errors (mp:process-name mp:*current-process*))
                             (describe-condition condition))))
  (mp:exit-process))

(defun install-guards (deadline)
  (setf *invoke-deadline* deadline)
  (setf (fdefinition 'ext:exit) #'no-exit)
  (setf (fdefinition 'ext:quit) #'no-exit)
  (setf *debugger-hook* #'unhandled)
  (setf ext:*invoke-debugger-hook* #'unhandled)
  ;; Nothing reads the gateway's stdin.
  (setf *standard-input* (make-string-input-stream ""))
  (setf *terminal-io* (make-two-way-stream *standard-input* *standard-output*)))

(defun main-loop (deadline)
  "The loop: never returns."
  (install-guards deadline)
  (send-json (obj "t" "booted" "version" (lisp-implementation-version)))
  (loop
    (handler-case
        (let ((m (mp:without-interrupts (veles-sys:%next 200))))
          (when m (dispatch (json-parse m)))
          (when *deadlines* (check-deadlines)))
      (serious-condition (e)
        (ignore-errors (host-log "error" (format nil "the loop: ~a" (describe-condition e))))))))
