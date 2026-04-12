use rand::thread_rng;
use std::env;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;

use ethers::prelude::{LocalWallet, Signer};
use ethers::types::Signature;
use ethers::types::transaction::eip712::TypedData;
use ethers::types::transaction::eip2718::TypedTransaction;

use zeroize::Zeroize;

use super::types::{Address, CoreError, TransactionRequest};
use tracing::{debug, instrument};

const MAX_MESSAGE_SIZE: usize = 1_000_000;
const MAX_ERROR_DISPLAY_LEN: usize = 150;
const REDACTED_HEX_STUB: &str = "0x***[REDACTED]***";
const ADDRESS_KEY: &str = "address";
const HEX_64_PATTERN: &str = r"0x[0-9a-fA-F]{64}";

/// Manages an Ethereum wallet and provides signing capabilities.
///
/// Supports loading from environment variables, keyfiles, or generating new keys.
#[derive(Clone)]
pub struct WalletManager {
    wallet: LocalWallet,
}

impl WalletManager {
    /// Loads a wallet from a private key stored in an environment variable.
    #[instrument(skip_all)]
    pub fn from_env(env_var: &str) -> Result<Self, WalletError> {
        debug!(env_var, "Loading wallet from environment variable");
        let mut raw_key =
            env::var(env_var).map_err(|_| WalletError::MissingKey(env_var.to_string()))?;
        let trimmed = raw_key.trim();

        let wallet = LocalWallet::from_str(trimmed).map_err(|e| {
            WalletError::InvalidKey(format!(
                "failed to parse private key: {}",
                type_name_of_error(&e)
            ))
        })?;

        raw_key.zeroize();
        Ok(Self { wallet })
    }

    /// Generates a new random wallet.
    pub fn generate() -> Result<Self, WalletError> {
        Ok(Self {
            wallet: LocalWallet::new(&mut thread_rng()),
        })
    }

    /// Decrypts a wallet from a standard Ethereum keystore file.
    pub fn from_keyfile<P: AsRef<Path>>(path: P, password: &str) -> Result<Self, WalletError> {
        let path_ref = path.as_ref();

        fs::metadata(path_ref)
            .map_err(|e| WalletError::KeyfileRead(format!("failed to read keyfile: {}", e)))?;

        let mut secret = eth_keystore::decrypt_key(path_ref, password).map_err(|e| {
            WalletError::KeyfileDecrypt(format!("failed to decrypt keyfile: {}", e))
        })?;

        let wallet = LocalWallet::from_bytes(&secret).map_err(|e| {
            WalletError::InvalidKey(format!(
                "invalid key in keyfile: {}",
                type_name_of_error(&e)
            ))
        })?;

        secret.zeroize();

        Ok(Self { wallet })
    }

    /// Exports the current wallet to an encrypted keystore file.
    #[instrument(skip(self, password, dir), fields(dir = ?dir.as_ref()))]
    pub fn to_keyfile<P: AsRef<Path>>(
        &self,
        dir: P,
        password: &str,
        name: Option<String>,
    ) -> Result<(), WalletError> {
        debug!("Exporting wallet to keyfile");
        let dir_path = dir.as_ref();
        let mut secret = self.wallet.signer().to_bytes();

        fs::create_dir_all(dir_path)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to create directory: {}", e)))?;

        let mut rng = thread_rng();
        let name_val = name.unwrap_or_else(|| {
            format!(
                "UTC--{}--{:x}",
                chrono::Utc::now().to_rfc3339().replace(':', "-"),
                self.wallet.address()
            )
        });

        let _uuid = eth_keystore::encrypt_key(
            dir_path,
            &mut rng,
            secret,
            password.as_bytes(),
            Some(&name_val),
        )
        .map_err(|e| WalletError::KeyfileEncrypt(format!("failed to encrypt keyfile: {}", e)))?;

        secret.zeroize();

        let keyfile_path = dir_path.join(&name_val);
        let content = fs::read_to_string(&keyfile_path).map_err(|e| {
            WalletError::KeyfileWrite(format!("failed to post-process keyfile: {}", e))
        })?;

        let mut json: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to parse keyfile: {}", e)))?;

        json[ADDRESS_KEY] = serde_json::json!(format!("{:x}", self.wallet.address()));

        let updated = serde_json::to_string_pretty(&json).map_err(|e| {
            WalletError::KeyfileWrite(format!("failed to serialize keyfile: {}", e))
        })?;

        fs::write(&keyfile_path, updated).map_err(|e| {
            WalletError::KeyfileWrite(format!("failed to write finalized keyfile: {}", e))
        })?;

        Ok(())
    }

    /// Returns the checksummed address string.
    pub fn address(&self) -> String {
        ethers::utils::to_checksum(&self.wallet.address(), None)
    }

    #[instrument(skip(self, message))]
    pub async fn sign_message(&self, message: &str) -> Result<Signature, WalletError> {
        debug!("Signing message ({} bytes)", message.len());
        if message.is_empty() {
            return Err(WalletError::EmptyMessage);
        }

        if message.len() > MAX_MESSAGE_SIZE {
            return Err(WalletError::MessageTooLarge(format!(
                "message exceeds maximum size of {} bytes",
                MAX_MESSAGE_SIZE
            )));
        }

        self.wallet
            .sign_message(message)
            .await
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))
    }

    /// Signs EIP-712 typed data.
    pub async fn sign_typed_data(&self, typed_data: TypedData) -> Result<Signature, WalletError> {
        self.wallet
            .sign_typed_data(&typed_data)
            .await
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))
    }

    /// Signs a transaction request and returns the signature.
    #[instrument(skip(self, tx))]
    pub async fn sign_transaction(
        &self,
        tx: &TransactionRequest,
    ) -> Result<Signature, WalletError> {
        debug!("Signing transaction");
        tx.validate()?;

        let request: TypedTransaction = tx.to_ethers_typed();
        self.wallet
            .sign_transaction(&request)
            .await
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))
    }

    /// Signs a transaction and returns the RLP-encoded signed bytes.
    pub async fn sign_transaction_bytes(
        &self,
        tx: &TransactionRequest,
    ) -> Result<Vec<u8>, WalletError> {
        tx.validate()?;

        let typed_tx: TypedTransaction = tx.to_ethers_typed();
        let signature = self
            .wallet
            .sign_transaction(&typed_tx)
            .await
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))?;

        Ok(typed_tx.rlp_signed(&signature).to_vec())
    }

    /// Verifies that a message was signed by the owner of a specific address.
    pub fn verify_signature(
        &self,
        message: &str,
        signature: &Signature,
    ) -> Result<Address, WalletError> {
        if message.is_empty() {
            return Err(WalletError::EmptyMessage);
        }

        let recovered = signature
            .recover(message)
            .map_err(|error| WalletError::Operation(error.to_string()))?;

        Address::new(ethers::utils::to_checksum(&recovered, None)).map_err(WalletError::from)
    }
}

impl fmt::Debug for WalletManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(stringify!(WalletManager))
            .field(ADDRESS_KEY, &self.address())
            .finish()
    }
}

impl fmt::Display for WalletManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({}={})",
            stringify!(WalletManager),
            ADDRESS_KEY,
            self.address()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalletError {
    #[error("missing private key environment variable: {0}")]
    MissingKey(String),
    #[error("invalid private key: {0}")]
    InvalidKey(String),
    #[error("message must not be empty")]
    EmptyMessage,
    #[error("{0}")]
    MessageTooLarge(String),
    #[error("failed to read keyfile: {0}")]
    KeyfileRead(String),
    #[error("invalid keyfile format: {0}")]
    KeyfileParse(String),
    #[error("failed to decrypt keyfile: {0}")]
    KeyfileDecrypt(String),
    #[error("failed to encrypt keyfile: {0}")]
    KeyfileEncrypt(String),
    #[error("failed to write keyfile: {0}")]
    KeyfileWrite(String),
    #[error("wallet operation failed: {0}")]
    Operation(String),
}

impl From<ethers::signers::WalletError> for WalletError {
    fn from(error: ethers::signers::WalletError) -> Self {
        Self::Operation(sanitize_error(&error))
    }
}

impl From<CoreError> for WalletError {
    fn from(error: CoreError) -> Self {
        Self::Operation(error.to_string())
    }
}

fn sanitize_error(error: &dyn std::error::Error) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let msg = error.to_string();

    let regex = RE.get_or_init(|| regex::Regex::new(HEX_64_PATTERN).unwrap());
    let sanitized = regex.replace_all(&msg, REDACTED_HEX_STUB);

    if sanitized.len() > MAX_ERROR_DISPLAY_LEN {
        format!("{}...", &sanitized[..MAX_ERROR_DISPLAY_LEN])
    } else {
        sanitized.to_string()
    }
}

fn type_name_of_error(_error: &dyn std::error::Error) -> &'static str {
    "key parsing error"
}
