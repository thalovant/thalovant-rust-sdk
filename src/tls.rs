use std::sync::Once;

static RUSTLS_PROVIDER: Once = Once::new();

pub(crate) fn ensure_rustls_provider() {
    RUSTLS_PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
