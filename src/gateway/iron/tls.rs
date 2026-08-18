//! Automatically managed mTLS identity for Iron's external transform client.
//! The generated Iron fragment references these files; it never embeds keys.

use crate::{Error, Result, index_path};
use fs2::FileExt;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

const TLS_STATE_VERSION: u8 = 1;

#[derive(Clone, Debug)]
pub(crate) struct IronTlsPaths {
    pub(crate) ca_cert: PathBuf,
    pub(crate) server_cert: PathBuf,
    pub(crate) server_key: PathBuf,
    pub(crate) client_cert: PathBuf,
    pub(crate) client_key: PathBuf,
    manifest: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct TlsState {
    version: u8,
    server_ip: IpAddr,
}

impl IronTlsPaths {
    pub(crate) fn in_directory(directory: &Path) -> Self {
        Self {
            ca_cert: directory.join("ca.pem"),
            server_cert: directory.join("server.pem"),
            server_key: directory.join("server-key.pem"),
            client_cert: directory.join("iron-client.pem"),
            client_key: directory.join("iron-client-key.pem"),
            manifest: directory.join("state.json"),
        }
    }

    pub(crate) fn server_config(&self) -> Result<ServerTlsConfig> {
        let cert = read(&self.server_cert)?;
        let key = read(&self.server_key)?;
        let client_ca = read(&self.ca_cert)?;
        Ok(ServerTlsConfig::new()
            .identity(Identity::from_pem(cert, key))
            .client_ca_root(Certificate::from_pem(client_ca)))
    }

    fn complete(&self) -> bool {
        [
            &self.ca_cert,
            &self.server_cert,
            &self.server_key,
            &self.client_cert,
            &self.client_key,
        ]
        .into_iter()
        .all(|path| path.is_file())
    }
}

pub(crate) fn ensure_iron_tls(server_ip: IpAddr) -> Result<IronTlsPaths> {
    let directory = iron_tls_directory()?;
    ensure_in(&directory, server_ip)
}

pub(crate) fn purge_iron_tls() -> Result<()> {
    let directory = iron_tls_directory()?;
    if directory.exists() {
        fs::remove_dir_all(&directory).map_err(|error| {
            Error::Message(format!(
                "could not remove Iron TLS material at {}: {error}",
                directory.display()
            ))
        })?;
    }
    Ok(())
}

fn iron_tls_directory() -> Result<PathBuf> {
    Ok(index_path()?
        .parent()
        .expect("index path has a parent")
        .join("iron-tls"))
}

fn ensure_in(directory: &Path, server_ip: IpAddr) -> Result<IronTlsPaths> {
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;

    let lock_path = directory.join("provision.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    let lock = options.open(&lock_path)?;
    FileExt::lock_exclusive(&lock).map_err(|error| {
        Error::Message(format!(
            "could not lock Iron TLS material at {}: {error}",
            lock_path.display()
        ))
    })?;

    let paths = IronTlsPaths::in_directory(directory);
    let current = fs::read(&paths.manifest)
        .ok()
        .and_then(|contents| serde_json::from_slice::<TlsState>(&contents).ok())
        .is_some_and(|state| {
            state.version == TLS_STATE_VERSION && state.server_ip == server_ip && paths.complete()
        });
    if !current {
        generate(&paths, server_ip)?;
    }
    Ok(paths)
}

fn generate(paths: &IronTlsPaths, server_ip: IpAddr) -> Result<()> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(cert_error)?;
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Subhub Iron local CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate().map_err(cert_error)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(cert_error)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().map_err(cert_error)?;
    let mut server_params = CertificateParams::new(vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
        server_ip.to_string(),
    ])
    .map_err(cert_error)?;
    server_params
        .distinguished_name
        .push(DnType::CommonName, "Subhub Iron transform service");
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.use_authority_key_identifier_extension = true;
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .map_err(cert_error)?;

    let client_key = KeyPair::generate().map_err(cert_error)?;
    let mut client_params = CertificateParams::new(Vec::<String>::new()).map_err(cert_error)?;
    client_params
        .distinguished_name
        .push(DnType::CommonName, "Iron Proxy");
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    client_params.use_authority_key_identifier_extension = true;
    let client_cert = client_params
        .signed_by(&client_key, &issuer)
        .map_err(cert_error)?;

    write_private(&paths.ca_cert, ca_cert.pem().as_bytes())?;
    write_private(&paths.server_cert, server_cert.pem().as_bytes())?;
    write_private(&paths.server_key, server_key.serialize_pem().as_bytes())?;
    write_private(&paths.client_cert, client_cert.pem().as_bytes())?;
    write_private(&paths.client_key, client_key.serialize_pem().as_bytes())?;
    write_private(
        &paths.manifest,
        &serde_json::to_vec_pretty(&TlsState {
            version: TLS_STATE_VERSION,
            server_ip,
        })?,
    )
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| {
        Error::Message(format!(
            "could not read Iron TLS file {}: {error}",
            path.display()
        ))
    })
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Message("Iron TLS path has no UTF-8 file name".into()))?;
    let temporary = path.with_file_name(format!(".{file_name}.tmp"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn cert_error(error: rcgen::Error) -> Error {
    Error::Message(format!("could not provision Iron mTLS identity: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::iron::IronTransform;
    use crate::gateway::iron::proto::TransformRequestRequest;
    use crate::gateway::iron::proto::transform_service_client::TransformServiceClient;
    use crate::gateway::iron::proto::transform_service_server::TransformServiceServer;
    use crate::gateway::state::test_state;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::{ClientTlsConfig, Endpoint, Server};

    fn test_directory() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "subhub-iron-tls-test-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn provisioning_is_stable_private_and_address_aware() {
        let directory = test_directory();
        let paths = ensure_in(&directory, "127.0.0.1".parse().unwrap()).unwrap();
        let first_client = fs::read(&paths.client_cert).unwrap();
        let first_server = fs::read(&paths.server_cert).unwrap();

        let same = ensure_in(&directory, "127.0.0.1".parse().unwrap()).unwrap();
        assert_eq!(fs::read(&same.client_cert).unwrap(), first_client);
        assert_eq!(fs::read(&same.server_cert).unwrap(), first_server);
        same.server_config().unwrap();

        for path in [
            &paths.ca_cert,
            &paths.server_cert,
            &paths.server_key,
            &paths.client_cert,
            &paths.client_key,
            &paths.manifest,
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let changed = ensure_in(&directory, "127.0.0.2".parse().unwrap()).unwrap();
        assert_ne!(fs::read(&changed.client_cert).unwrap(), first_client);
        assert_ne!(fs::read(&changed.server_cert).unwrap(), first_server);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn transform_service_requires_the_generated_client_certificate() {
        let directory = test_directory();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let paths = ensure_in(&directory, address.ip()).unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_tls = paths.server_config().unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(server_tls)
                .unwrap()
                .add_service(TransformServiceServer::new(
                    IronTransform::new(test_state()),
                ))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let uri = format!("https://{address}");
        let ca = fs::read(&paths.ca_cert).unwrap();
        let unauthenticated = Endpoint::from_shared(uri.clone())
            .unwrap()
            .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca.clone())))
            .unwrap();
        let unauthenticated = unauthenticated.connect().await.unwrap();
        assert!(
            TransformServiceClient::new(unauthenticated)
                .transform_request(TransformRequestRequest::default())
                .await
                .is_err()
        );

        let client_identity = Identity::from_pem(
            fs::read(&paths.client_cert).unwrap(),
            fs::read(&paths.client_key).unwrap(),
        );
        let authenticated = Endpoint::from_shared(uri)
            .unwrap()
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(ca))
                    .identity(client_identity),
            )
            .unwrap()
            .connect()
            .await
            .unwrap();
        TransformServiceClient::new(authenticated)
            .transform_request(TransformRequestRequest::default())
            .await
            .unwrap();

        let _ = shutdown_tx.send(());
        server.await.unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
