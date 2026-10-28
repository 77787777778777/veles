//! The `veles-plugin-atproto` executable: the gateway spawns it, it speaks
//! the plugin protocol on stdin/stdout. `--describe` prints the manifest
//! (the config surface, the generic repo services).
//! All the work is in the library (`AtprotoAdapter`); this is the shell.

fn main() -> std::process::ExitCode {
    // Before any TLS config is built (reqwest, the Jetstream socket):
    // the provider is this process's choice, not the build's.
    veles_atproto::install_crypto_provider();
    veles_plugin_sdk::run(
        veles_atproto::manifest(),
        veles_atproto::AtprotoAdapter::new(),
    )
}
