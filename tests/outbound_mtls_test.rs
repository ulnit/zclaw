//! End-to-end proof for outbound mTLS.
//!
//! A real rustls server on 127.0.0.1 that *requires* a client certificate:
//!
//! 1. a reqwest client built through [`outbound_tls::apply_env`] is accepted,
//! 2. the same client *without* its identity is refused (so (1) is not vacuous),
//! 3. `mtls probe` classifies the endpoint as `enforced`,
//! 4. the same probe on a server that does not ask for a client certificate
//!    reports `not_enforced` — i.e. the verdict distinguishes the two cases.
//!
//! The PKI under `tests/fixtures/mtls/` is throwaway test material generated
//! with openssl (see `tests/fixtures/mtls/README.md`); nothing here is a real
//! credential.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use ulnclaw::outbound_tls::{
    self, ClientAuthVerdict, OutboundTls, ENV_CA, ENV_CERT, ENV_HOSTS, ENV_KEY, ENV_MIN_TLS,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mtls")
        .join(name)
}

fn certs(path: &Path) -> Vec<CertificateDer<'static>> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    rustls_pemfile::certs(&mut std::io::BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .expect("parse certs")
}

fn key(path: &Path) -> PrivateKeyDer<'static> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    rustls_pemfile::private_key(&mut std::io::BufReader::new(file))
        .expect("parse key")
        .expect("key present")
}

/// The test harness drives raw `rustls` directly, so pin the crypto provider
/// instead of relying on rustls inferring one from crate features (feature
/// unification with `reqwest`/`tokio-rustls` can defeat the auto-detection).
fn install_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::aws_lc_rs::default_provider(),
        );
    }
}

fn server_config(require_client_auth: bool) -> rustls::ServerConfig {
    install_provider();
    let chain = certs(&fixture("server.crt"));
    let server_key = key(&fixture("server.key"));
    let builder = rustls::ServerConfig::builder();
    if require_client_auth {
        let mut roots = rustls::RootCertStore::empty();
        let _ = roots.add_parsable_certificates(certs(&fixture("ca.crt")));
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .expect("client verifier");
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, server_key)
            .expect("server config")
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(chain, server_key)
            .expect("server config")
    }
}

/// Minimal HTTPS responder; the handshake is what the test cares about.
async fn spawn_server(require_client_auth: bool) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let acceptor = TlsAcceptor::from(Arc::new(server_config(require_client_auth)));
    let handle = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(tcp).await {
                    // TLS 1.3 lets the anonymous handshake complete; the client
                    // certificate is only examined afterwards, so drop the
                    // connection the way a real terminator does.
                    let presented = tls
                        .get_ref()
                        .1
                        .peer_certificates()
                        .map(|chain| !chain.is_empty())
                        .unwrap_or(false);
                    if require_client_auth && !presented {
                        let _ = tls.shutdown().await;
                        return;
                    }
                    let mut buf = [0u8; 2048];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.flush().await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    (port, handle)
}

fn policy(hosts: &str) -> OutboundTls {
    let pairs: Vec<(String, String)> = vec![
        (ENV_CERT.into(), fixture("client.crt").to_string_lossy().into_owned()),
        (ENV_KEY.into(), fixture("client.key").to_string_lossy().into_owned()),
        (ENV_CA.into(), fixture("ca.crt").to_string_lossy().into_owned()),
        (ENV_HOSTS.into(), hosts.to_string()),
        (ENV_MIN_TLS.into(), "1.2".to_string()),
    ];
    OutboundTls::from_lookup(&move |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    })
    .expect("policy resolves")
    .expect("policy enabled")
}

async fn get(url: &str, client: &reqwest::Client) -> Result<reqwest::Response, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{e}"))
}

#[tokio::test]
async fn identity_is_accepted_when_server_requires_mtls() {
    let (port, server) = spawn_server(true).await;
    let url = format!("https://127.0.0.1:{port}/");
    let policy = policy("127.0.0.1");
    assert!(policy.applies_to("127.0.0.1"));

    let builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(false);
    let builder = policy.apply(builder, "127.0.0.1").expect("apply policy");
    let client = builder.build().expect("build client");

    match get(&url, &client).await {
        Ok(resp) => assert_eq!(resp.status().as_u16(), 200, "mTLS request succeeded"),
        Err(e) => {
            server.abort();
            panic!("mTLS request failed although the server trusts our CA: {e}");
        }
    }
    server.abort();
}

#[tokio::test]
async fn anonymous_client_is_refused_when_server_requires_mtls() {
    let (port, server) = spawn_server(true).await;
    let url = format!("https://127.0.0.1:{port}/");

    // Same trust anchor, no client identity: the endpoint must reject us.
    let ca = reqwest::Certificate::from_pem(&std::fs::read(fixture("ca.crt")).unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .add_root_certificate(ca)
        .build()
        .expect("build client");

    let err = get(&url, &client)
        .await
        .expect_err("anonymous request must fail — otherwise the mTLS test proves nothing");
    assert!(
        !err.is_empty(),
        "refusal should carry a TLS error, got {err:?}"
    );
    server.abort();
}

#[tokio::test]
async fn probe_reports_enforced_against_mtls_server() {
    let (port, server) = spawn_server(true).await;
    let url = format!("https://127.0.0.1:{port}/");
    let policy = policy("127.0.0.1");

    let report = outbound_tls::probe_with(&url, Duration::from_secs(10), Some(&policy))
        .await
        .expect("probe runs");
    assert_eq!(
        report.verdict,
        ClientAuthVerdict::Enforced,
        "probe text:\n{}",
        report.to_text()
    );
    assert!(report.ok());
    assert!(report.with_identity.as_ref().expect("identity arm").ok);
    assert_eq!(
        report.host_in_scope,
        Some(true),
        "127.0.0.1 is inside ULNCLAW_MTLS_HOSTS here, so the agent itself would use the identity"
    );
    assert!(
        matches!(
            report.without_identity.request,
            outbound_tls::AfterHandshake::Refused(_)
        ),
        "anonymous arm should be refused after the handshake, got {:?}",
        report.without_identity.request
    );
    server.abort();
}

#[tokio::test]
async fn probe_reports_not_enforced_against_plain_tls_server() {
    let (port, server) = spawn_server(false).await;
    let url = format!("https://127.0.0.1:{port}/");
    let policy = policy("127.0.0.1");

    let report = outbound_tls::probe_with(&url, Duration::from_secs(10), Some(&policy))
        .await
        .expect("probe runs");
    assert_eq!(report.verdict, ClientAuthVerdict::NotRequired);
    assert!(!report.ok(), "not-required must not pass as mTLS");
    assert_eq!(
        report.without_identity.request,
        outbound_tls::AfterHandshake::Served(200),
        "the plain server should have answered the anonymous request"
    );
    server.abort();
}

#[tokio::test]
async fn probe_refuses_plain_http() {
    let err = outbound_tls::probe_with("http://127.0.0.1:9/", Duration::from_secs(1), None)
        .await
        .expect_err("http must be refused");
    assert!(err.contains("https"), "{err}");
}

/// The probe deliberately presents the identity even when the target is outside
/// ULNCLAW_MTLS_HOSTS, so it must label that case: otherwise an endpoint the
/// agent would never send its certificate to looks identical to the real
/// gateway (same verdict, same wording).
#[tokio::test]
async fn probe_labels_targets_outside_the_configured_scope() {
    let (port, server) = spawn_server(true).await;
    let url = format!("https://127.0.0.1:{port}/");
    let policy = policy("gw.example.com");
    assert!(
        !policy.applies_to("127.0.0.1"),
        "precondition: the test scope must not cover the loopback target"
    );

    let report = outbound_tls::probe_with(&url, Duration::from_secs(10), Some(&policy))
        .await
        .expect("probe runs");
    assert_eq!(report.host_in_scope, Some(false));
    assert!(
        report.to_text().contains("OUT OF SCOPE"),
        "text report must warn about the scope:\n{}",
        report.to_text()
    );
    assert!(
        report
            .to_json()
            .to_string()
            .contains("\"host_in_scope\":false"),
        "json report must carry host_in_scope=false: {}",
        report.to_json()
    );
    server.abort();
}
