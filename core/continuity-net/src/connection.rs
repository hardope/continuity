use continuity_crypto::TlsIdentity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{client::TlsStream as ClientTlsStream, server::TlsStream as ServerTlsStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("peer presented no certificate")]
    NoPeerCertificate,
    #[error("peer certificate is not a well-formed Ed25519 SPKI cert: {0}")]
    BadCertificate(String),
}

/// Either side of an established, mutually-authenticated connection: we
/// know cryptographically that the peer controls the private key behind
/// `peer_device_id`, but whether that id is *trusted* is a separate,
/// application-level check (see `sync::SyncEngine`) — the same
/// trust-on-first-use split SSH makes between "handshake succeeded" and
/// "host key is known."
pub enum Connection {
    Server(ServerTlsStream<TcpStream>),
    Client(ClientTlsStream<TcpStream>),
}

impl Connection {
    pub fn peer_device_id(&self) -> Result<String, ConnectionError> {
        let cert = match self {
            Connection::Server(stream) => stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first()),
            Connection::Client(stream) => stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first()),
        }
        .ok_or(ConnectionError::NoPeerCertificate)?;

        device_id_from_cert(cert)
    }
}

impl tokio::io::AsyncRead for Connection {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Connection::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Connection::Client(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for Connection {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Connection::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Connection::Client(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Connection::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
            Connection::Client(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Connection::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Connection::Client(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

fn device_id_from_cert(cert: &CertificateDer<'_>) -> Result<String, ConnectionError> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|e| ConnectionError::BadCertificate(e.to_string()))?;
    let raw_key = parsed.public_key().raw;
    // Ed25519 SubjectPublicKeyInfo is a 1-byte-aligned BIT STRING, so `raw`
    // is either the bare 32-byte key or that plus a leading 0x00
    // "unused bits" byte depending on the parser — normalize by taking the
    // last 32 bytes rather than assuming which.
    let key_bytes = if raw_key.len() >= 32 {
        &raw_key[raw_key.len() - 32..]
    } else {
        raw_key
    };
    Ok(hex::encode(key_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuity_crypto::{generate_self_signed, Identity};

    #[test]
    fn device_id_from_cert_matches_identity_device_id() {
        let identity = Identity::generate();
        let tls = generate_self_signed(&identity).expect("generate cert");
        let from_cert = device_id_from_cert(&tls.cert_der).expect("parse cert");
        assert_eq!(from_cert, identity.device_id());
    }
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts a self-signed cert from anyone but still performs the full
/// cryptographic signature check via the delegated `verify_tls*` calls —
/// only the "does this chain to a trusted root" step is skipped, since we
/// have no CA and don't want one (see module docs on `Connection`).
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct AcceptAnyClientCert(Arc<rustls::crypto::CryptoProvider>);

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(identity: &TlsIdentity) -> Result<rustls::ClientConfig, ConnectionError> {
    let provider = crypto_provider();
    let verifier = Arc::new(AcceptAnyServerCert(provider.clone()));
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )?;
    Ok(config)
}

fn server_config(identity: &TlsIdentity) -> Result<rustls::ServerConfig, ConnectionError> {
    let provider = crypto_provider();
    let client_verifier = Arc::new(AcceptAnyClientCert(provider.clone()));
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )?;
    Ok(config)
}

/// Binds and listens for incoming peer connections, performing the TLS
/// handshake (with mutual client-cert auth) for each accepted socket.
pub struct Listener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl Listener {
    pub async fn bind(addr: SocketAddr, identity: &TlsIdentity) -> Result<Self, ConnectionError> {
        let tcp = TcpListener::bind(addr).await?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config(identity)?));
        Ok(Self { tcp, acceptor })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, ConnectionError> {
        Ok(self.tcp.local_addr()?)
    }

    pub async fn accept(&self) -> Result<(Connection, SocketAddr), ConnectionError> {
        let (tcp_stream, peer_addr) = self.tcp.accept().await?;
        let tls_stream = self.acceptor.accept(tcp_stream).await?;
        Ok((Connection::Server(tls_stream), peer_addr))
    }
}

/// Dials a peer discovered via mDNS and performs the client-side TLS
/// handshake (also presenting our own cert, for mutual auth).
pub async fn connect(addr: SocketAddr, identity: &TlsIdentity) -> Result<Connection, ConnectionError> {
    let connector = TlsConnector::from(Arc::new(client_config(identity)?));
    let tcp_stream = TcpStream::connect(addr).await?;
    // Server name is unused for verification (see AcceptAnyServerCert) but
    // still required by the rustls API shape; any well-formed name works.
    let server_name = ServerName::try_from("continuity.local")
        .map_err(|_| ConnectionError::BadCertificate("invalid server name".into()))?
        .to_owned();
    let tls_stream = connector.connect(server_name, tcp_stream).await?;
    Ok(Connection::Client(tls_stream))
}
