use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf, sync::Arc};
use zeroize::Zeroizing;

pub type DeviceId = [u8; 16];
pub type Token = [u8; 32];
pub const TLS_NAME: &str = "mouse-share.local";
const SERVICE: &str = "com.xunfei.mouse-share.v2";

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("System randomness: {e}"))?;
    Ok(bytes)
}

pub fn display_id(id: DeviceId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Features {
    pub keyboard: bool,
    pub clipboard: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            keyboard: true,
            clipboard: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PairedDevice {
    pub id: DeviceId,
    pub name: String,
    /// Present for a saved server, absent for a client authorized by this device.
    pub address: Option<String>,
    pub certificate: Vec<u8>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Invite {
    pub server: PairedDevice,
    pub token: Token,
}

impl Invite {
    pub fn encode(&self) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(format!(
            "MS2-{}",
            URL_SAFE_NO_PAD.encode(Zeroizing::new(serde_json::to_vec(self)?).as_slice())
        )))
    }

    pub fn decode(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(text.len() <= 16_384, "Invitation is too large");
        let body = text
            .strip_prefix("MS2-")
            .context("Expected a v2 pairing invitation")?;
        let invite: Self = serde_json::from_slice(&Zeroizing::new(URL_SAFE_NO_PAD.decode(body)?))
            .map_err(|_| anyhow::anyhow!("Invalid pairing invitation"))?;
        ensure!(
            invite.server.address.is_some() && !invite.server.certificate.is_empty(),
            "Invitation is missing the server identity"
        );
        Ok(invite)
    }
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Zeroizing<Vec<u8>>>;
    fn put(&self, key: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

pub struct SystemCredentials;
impl CredentialStore for SystemCredentials {
    fn get(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        Ok(Zeroizing::new(
            keyring::Entry::new(SERVICE, key)?.get_secret()?,
        ))
    }
    fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        keyring::Entry::new(SERVICE, key)?.set_secret(value)?;
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        match keyring::Entry::new(SERVICE, key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Data {
    id: Option<DeviceId>,
    certificate: Vec<u8>,
    devices: Vec<PairedDevice>,
    features: Features,
}

#[derive(Clone)]
pub struct Devices {
    dir: PathBuf,
    credentials: Arc<dyn CredentialStore>,
}

impl Devices {
    pub fn system() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("com", "xunfei", "mouse-share")
            .context("Cannot locate the mouse-share configuration directory")?;
        Ok(Self::new(
            dirs.config_dir().to_owned(),
            Arc::new(SystemCredentials),
        ))
    }

    pub fn new(dir: PathBuf, credentials: Arc<dyn CredentialStore>) -> Self {
        Self { dir, credentials }
    }

    fn load(&self) -> Result<Data> {
        match fs::read(self.dir.join("devices-v2.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Data::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn update<T>(&self, f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.dir)?;
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join("devices-v2.lock"))?;
        lock.lock()?;
        let mut data = self.load()?;
        let value = f(&mut data)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(&mut file, &data)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join("devices-v2.json"))?;
        Ok(value)
    }

    pub fn id(&self) -> Result<DeviceId> {
        if let Some(id) = self.load()?.id {
            return Ok(id);
        }
        self.update(|data| match data.id {
            Some(id) => Ok(id),
            None => {
                let id = random()?;
                data.id = Some(id);
                Ok(id)
            }
        })
    }

    pub fn identity(&self) -> Result<(DeviceId, Vec<u8>, Zeroizing<Vec<u8>>)> {
        let id = self.id()?;
        let key = format!("identity-{}", display_id(id));
        let cert = self.update(|data| {
            if data.certificate.is_empty() {
                let generated = rcgen::generate_simple_self_signed(vec![TLS_NAME.into()])?;
                self.credentials.put(
                    &key,
                    Zeroizing::new(generated.signing_key.serialize_der()).as_slice(),
                )?;
                data.certificate = generated.cert.der().to_vec();
            }
            Ok(data.certificate.clone())
        })?;
        Ok((
            id,
            cert,
            self.credentials
                .get(&key)
                .context("Cannot read the server identity from the system credential store")?,
        ))
    }

    pub fn list(&self) -> Result<Vec<PairedDevice>> {
        Ok(self.load()?.devices)
    }

    pub fn features(&self) -> Result<Features> {
        Ok(self.load()?.features)
    }

    pub fn set_features(&self, features: Features) -> Result<()> {
        self.update(|data| {
            data.features = features;
            Ok(())
        })
    }

    fn token_key(&self, id: DeviceId) -> Result<String> {
        Ok(format!(
            "peer-{}-{}",
            display_id(self.id()?),
            display_id(id)
        ))
    }

    pub fn token(&self, id: DeviceId) -> Result<Zeroizing<Token>> {
        let secret = self.credentials.get(&self.token_key(id)?)?;
        Ok(Zeroizing::new(secret.as_slice().try_into().map_err(
            |_| anyhow::anyhow!("Invalid saved credential"),
        )?))
    }

    pub fn add(&self, device: PairedDevice, token: &Token) -> Result<()> {
        let key = self.token_key(device.id)?;
        let mut written = false;
        let result = self.update(|data| {
            ensure!(
                !data.devices.iter().any(|peer| peer.id == device.id),
                "Forget the existing device before pairing it again"
            );
            self.credentials.put(&key, token)?;
            written = true;
            data.devices.push(device);
            Ok(())
        });
        if result.is_err() && written {
            let _ = self.credentials.delete(&key);
        }
        result
    }

    pub fn forget(&self, id: DeviceId) -> Result<()> {
        let key = self.token_key(id)?;
        self.update(|data| {
            data.devices.retain(|device| device.id != id);
            Ok(())
        })?;
        self.credentials.delete(&key)
    }

    pub fn set_address(&self, id: DeviceId, address: String) -> Result<()> {
        self.update(|data| {
            let device = data
                .devices
                .iter_mut()
                .find(|d| d.id == id)
                .context("Unknown device")?;
            if device.address.is_none() {
                bail!("Only saved servers have a connection address");
            }
            device.address = Some(address);
            Ok(())
        })
    }
}

pub fn local_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.into())
        .chars()
        .take(64)
        .collect()
}
