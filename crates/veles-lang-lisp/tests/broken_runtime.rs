//! Its own process (ECL boots once per process): a runtime that does not
//! even load leaves the engine dead with the reason, never the process
//! gone.

use veles_lang_lisp::LispEngine;
use veles_runtime::tokio;

#[test]
fn a_runtime_that_does_not_load_leaves_the_engine_dead_not_the_process() {
    let state = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(LispEngine::get_or_boot_with(
        rt.handle().clone(),
        state.path(),
        "(defpackage #:veles-runtime (:use #:cl))\n(in-package #:veles-runtime)\n(defun broken (\n",
    ));
    // The engine thread has returned here, but the shutdown is the
    // module host's way regardless (a plain drop waits for that thread).
    rt.shutdown_background();
    let err = r.err().expect("a runtime that does not load boots nothing");
    assert!(err.contains("ECL failed to boot"), "{err}");
    assert!(
        err.to_lowercase().contains("end of file"),
        "the reason is the runtime's own error, not a generic one: {err}"
    );
}
