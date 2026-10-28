//! Its own process (Racket boots once per process): a runtime that does
//! not even read leaves the engine dead with the reason, never the
//! process gone. Racket's own escape handler at boot EXITS the process,
//! with status 0, which a mutant run read as green (2026-10-10).

use veles_lang_racket::RacketEngine;
use veles_runtime::tokio;

#[test]
fn a_runtime_that_does_not_read_leaves_the_engine_dead_not_the_process() {
    let state = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(RacketEngine::get_or_boot_with(
        rt.handle().clone(),
        state.path(),
        "#lang racket/base\n(define (broken\n",
    ));
    // The engine thread has returned here, but the shutdown is the
    // module host's way regardless (a plain drop waits for that thread).
    rt.shutdown_background();
    let err = r.err().expect("a runtime that does not read boots nothing");
    assert!(err.contains("Racket failed to boot"), "{err}");
    assert!(
        err.contains("read-syntax"),
        "the reason is the runtime's own error, not a generic one: {err}"
    );
}
