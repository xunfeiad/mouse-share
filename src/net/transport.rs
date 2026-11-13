use super::devices::TLS_NAME;
use anyhow::{ensure, Context, Result};
use quinn::{
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
    ClientConfig, Endpoint, RecvStream, SendStream,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::{de::DeserializeOwned, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

pub const ALPN: &[u8] = b"mouse-share/2";
pub const HANDSHAKE: Duration = Duration::from_secs(5);
const FRAME_LIMIT: usize = 64 * 1024;

fn parameters() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config
        .max_concurrent_bidi_streams(3_u8.into())
        .max_concurrent_uni_streams(0_u8.into())
        .max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()))
        .datagram_receive_buffer_size(Some(16 * 1024))
        .datagram_send_buffer_size(4096);
    Arc::new(config)
}

pub fn server(bind: SocketAddr, cert: Vec<u8>, key: Vec<u8>) -> Result<Endpoint> {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert)],
        PrivatePkcs8KeyDer::from(key).into(),
    )?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport = parameters();
    Ok(Endpoint::server(config, bind)?)
}

pub async fn connect(address: &str, certificate: Vec<u8>) -> Result<(Endpoint, quinn::Connection)> {
    connect_with_alpn(address, certificate, ALPN).await
}

pub(super) async fn connect_with_alpn(
    address: &str,
    certificate: Vec<u8>,
    alpn: &[u8],
) -> Result<(Endpoint, quinn::Connection)> {
    let addr = tokio::time::timeout(HANDSHAKE, tokio::net::lookup_host(address))
        .await??
        .find(SocketAddr::is_ipv4)
        .context("Expected an IPv4 address or hostname and port")?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(certificate))?;
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.enable_early_data = false;
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(parameters());
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(config);
    let connection = tokio::time::timeout(HANDSHAKE, endpoint.connect(addr, TLS_NAME)?)
        .await
        .context(
            "Connection timed out. Check the address and ensure both devices run protocol v2",
        )??;
    Ok((endpoint, connection))
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    let data = serde_json::to_vec(message)?;
    ensure!(data.len() <= FRAME_LIMIT, "Control frame is too large");
    Ok(data)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(bytes.len() <= FRAME_LIMIT, "Control frame is too large");
    Ok(serde_json::from_slice(bytes)?)
}

pub async fn send<T: Serialize>(stream: &mut SendStream, message: &T) -> Result<()> {
    let data = zeroize::Zeroizing::new(encode(message)?);
    stream.write_all(&(data.len() as u32).to_be_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}

pub async fn receive<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<T> {
    let len = stream.read_u32().await? as usize;
    ensure!(
        (1..=FRAME_LIMIT).contains(&len),
        "Invalid control frame length"
    );
    let mut data = zeroize::Zeroizing::new(vec![0; len]);
    stream.read_exact(&mut data).await?;
    decode(&data)
}

pub fn encode_position(position: &crate::protocol::Position) -> [u8; 40] {
    let mut bytes = [0; 40];
    for (chunk, value) in bytes.chunks_exact_mut(8).zip([
        position.epoch,
        position.seq,
        position.control,
        position.point.0.to_bits(),
        position.point.1.to_bits(),
    ]) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    bytes
}

pub fn decode_position(bytes: &[u8]) -> Option<crate::protocol::Position> {
    let bytes: &[u8; 40] = bytes.try_into().ok()?;
    let mut parts = bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_be_bytes(chunk.try_into().unwrap()));
    let position = crate::protocol::Position {
        epoch: parts.next()?,
        seq: parts.next()?,
        control: parts.next()?,
        point: (f64::from_bits(parts.next()?), f64::from_bits(parts.next()?)),
    };
    (position.point.0.is_finite() && position.point.1.is_finite()).then_some(position)
}
