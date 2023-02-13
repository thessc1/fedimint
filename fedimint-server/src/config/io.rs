use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use aead::{encrypted_read, encrypted_write, get_key, LessSafeKey};
use anyhow::{ensure, format_err};
use bitcoin_hashes::hex::{FromHex, ToHex};
use fedimint_api::config::{ConfigGenParams, ModuleGenRegistry};
use fedimint_api::task::TaskGroup;
use fedimint_api::PeerId;
use fedimint_core::api::WsClientConnectInfo;
use itertools::Itertools;
use rand::rngs::OsRng;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_rustls::rustls;
use url::Url;

use crate::config::{
    connect, gen_cert_and_key, PeerServerParams, ServerConfig, ServerConfigParams,
};
use crate::fedimint_api::net::peers::IMuxPeerConnections;
use crate::multiplexed::PeerConnectionMultiplexer;

/// Client configuration file
pub const CLIENT_CONFIG: &str = "client";

/// Server encrypted private keys file
pub const PRIVATE_CONFIG: &str = "private";

/// Server locally configurable file
pub const LOCAL_CONFIG: &str = "local";

/// Server consensus-only configurable file
pub const CONSENSUS_CONFIG: &str = "consensus";

/// Client connection string file
pub const CLIENT_CONNECT_FILE: &str = "client-connect";

/// Salt backup for combining with the private key
pub const SALT_FILE: &str = "private.salt";

/// Database file name
pub const DB_FILE: &str = "database";

/// Encrypted TLS private keys
pub const TLS_PK: &str = "tls-pk";

/// TLS public cert
pub const TLS_CERT: &str = "tls-cert";

pub const JSON_EXT: &str = "json";
const ENCRYPTED_EXT: &str = "encrypt";

pub fn create_cert(
    dir_out_path: PathBuf,
    p2p_url: Url,
    api_url: Url,
    guardian_name: String,
    password: Option<String>,
) -> anyhow::Result<String> {
    let salt: [u8; 16] = rand::random();
    fs::write(dir_out_path.join(SALT_FILE), salt.to_hex())?;
    let key = get_key(password, dir_out_path.join(SALT_FILE))?;
    gen_tls(&dir_out_path, p2p_url, api_url, guardian_name, &key)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_dkg(
    bind_p2p: SocketAddr,
    bind_api: SocketAddr,
    dir_out_path: &Path,
    federation_name: String,
    certs: Vec<String>,
    pk: rustls::PrivateKey,
    task_group: &mut TaskGroup,
    code_version: &str,
    module_params: ConfigGenParams,
    module_registry: ModuleGenRegistry,
) -> anyhow::Result<ServerConfig> {
    let mut peers = BTreeMap::<PeerId, PeerServerParams>::new();
    for (idx, cert) in certs.into_iter().sorted().enumerate() {
        peers.insert(PeerId::from(idx as u16), parse_peer_params(cert)?);
    }

    let cert_string = fs::read_to_string(dir_out_path.join(TLS_CERT))?;

    let our_params = parse_peer_params(cert_string)?;
    let our_id = peers
        .iter()
        .find(|(_peer, params)| params.cert == our_params.cert)
        .map(|(peer, _)| *peer)
        .ok_or_else(|| anyhow::Error::msg("Our id not found"))?;

    let params = ServerConfigParams::gen_params(
        bind_p2p,
        bind_api,
        pk,
        our_id,
        &peers,
        federation_name,
        module_params,
    );

    let peer_ids: Vec<PeerId> = peers.keys().cloned().collect();
    let server_conn = connect(params.fed_network.clone(), params.tls.clone(), task_group).await;

    let connections = PeerConnectionMultiplexer::new(server_conn).into_dyn();

    let result = ServerConfig::distributed_gen(
        code_version,
        &connections,
        &our_id,
        &peer_ids,
        &params,
        module_registry,
        OsRng,
        task_group,
    )
    .await?;

    drop(connections);

    Ok(result?)
}

pub fn parse_peer_params(url: String) -> anyhow::Result<PeerServerParams> {
    let split: Vec<&str> = url.split('@').collect();

    ensure!(split.len() == 4, "Cert string has wrong number of fields");
    let p2p_url = split[0].parse()?;
    let api_url = split[1].parse()?;
    let hex_cert = Vec::from_hex(split[3])?;
    Ok(PeerServerParams {
        cert: rustls::Certificate(hex_cert),
        p2p_url,
        api_url,
        name: split[2].to_string(),
    })
}

fn gen_tls(
    dir_out_path: &Path,
    p2p_url: Url,
    api_url: Url,
    name: String,
    key: &LessSafeKey,
) -> anyhow::Result<String> {
    let (cert, pk) = gen_cert_and_key(&name)?;
    encrypted_write(pk.0, key, dir_out_path.join(TLS_PK))?;

    rustls::ServerName::try_from(name.as_str())?;
    // TODO Base64 encode name, hash fingerprint cert_string
    let cert_url = format!("{}@{}@{}@{}", p2p_url, api_url, name, cert.0.to_hex());
    fs::write(dir_out_path.join(TLS_CERT), &cert_url)?;
    Ok(cert_url)
}

/// Reads the server from the local, private, and consensus cfg files
/// (private file encrypted)
pub fn read_server_configs(key: &LessSafeKey, path: PathBuf) -> anyhow::Result<ServerConfig> {
    Ok(ServerConfig {
        consensus: plaintext_json_read(path.join(CONSENSUS_CONFIG))?,
        local: plaintext_json_read(path.join(LOCAL_CONFIG))?,
        private: encrypted_json_read(key, path.join(PRIVATE_CONFIG))?,
    })
}

/// Reads a plaintext json file into a struct
pub fn plaintext_json_read<T: Serialize + DeserializeOwned>(path: PathBuf) -> anyhow::Result<T> {
    let string = fs::read_to_string(path.with_extension(JSON_EXT))?;
    Ok(serde_json::from_str(&string)?)
}

/// Reads an encrypted json file into a struct
pub fn encrypted_json_read<T: Serialize + DeserializeOwned>(
    key: &LessSafeKey,
    path: PathBuf,
) -> anyhow::Result<T> {
    let decrypted = encrypted_read(key, path.with_extension(ENCRYPTED_EXT));
    let string = String::from_utf8(decrypted?)?;
    Ok(serde_json::from_str(&string)?)
}

/// Writes the server into plaintext json configuration files
/// (private keys not serialized)
pub fn write_nonprivate_configs(
    server: &ServerConfig,
    path: PathBuf,
    module_config_gens: &ModuleGenRegistry,
) -> anyhow::Result<()> {
    let client_config = server
        .consensus
        .to_config_response(module_config_gens)
        .client;
    plaintext_json_write(&server.local, path.join(LOCAL_CONFIG))?;
    plaintext_json_write(&server.consensus, path.join(CONSENSUS_CONFIG))?;
    plaintext_json_write(
        &WsClientConnectInfo::from_honest_peers(&client_config),
        path.join(CLIENT_CONNECT_FILE),
    )?;
    plaintext_json_write(&client_config, path.join(CLIENT_CONFIG))
}

/// Writes struct into a plaintext json file
pub fn plaintext_json_write<T: Serialize + DeserializeOwned>(
    obj: &T,
    path: PathBuf,
) -> anyhow::Result<()> {
    let filename = path.with_extension(JSON_EXT);
    let file = fs::File::create(filename.clone())
        .map_err(|_| format_err!("Unable to create file {:?}", filename))?;
    serde_json::to_writer_pretty(file, obj)?;
    Ok(())
}

/// Writes struct into an encrypted json file
pub fn encrypted_json_write<T: Serialize + DeserializeOwned>(
    obj: &T,
    key: &LessSafeKey,
    path: PathBuf,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_string(obj)?.into_bytes();
    encrypted_write(bytes, key, path.with_extension(ENCRYPTED_EXT))
}