use std::env;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use ethers::prelude::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::transaction::eip712::TypedData;
use ethers::types::Signature;
use rand::thread_rng;
use tokio::runtime::Runtime;
use zeroize::Zeroize;

use super::types::{Address, CoreError, TransactionRequest};

const MAX_MESSAGE_SIZE: usize = 1_000_000;

#[derive(Clone)]
pub struct WalletManager {
    wallet: LocalWallet,
}

impl WalletManager {
    pub fn from_env(env_var: &str) -> Result<Self, WalletError> {
        let mut raw_key = env::var(env_var).map_err(|_| WalletError::MissingKey(env_var.to_string()))?;
        let trimmed = raw_key.trim();
        
        let wallet = LocalWallet::from_str(trimmed)
            .map_err(|e| WalletError::InvalidKey(format!("failed to parse private key: {}", type_name_of_error(&e))))?;

        raw_key.zeroize();
        Ok(Self { wallet })
    }

    pub fn generate() -> Result<Self, WalletError> {
        Ok(Self {
            wallet: LocalWallet::new(&mut thread_rng()),
        })
    }

    pub fn from_keyfile<P: AsRef<Path>>(path: P, password: &str) -> Result<Self, WalletError> {
        let path_ref = path.as_ref();

        fs::metadata(path_ref)
            .map_err(|e| WalletError::KeyfileRead(format!("failed to read keyfile: {}", e)))?;

        let secret = eth_keystore::decrypt_key(path_ref, password)
            .map_err(|e| WalletError::KeyfileDecrypt(format!("failed to decrypt keyfile: {}", e)))?;

        let wallet = LocalWallet::from_bytes(&secret)
            .map_err(|e| WalletError::InvalidKey(format!("invalid key in keyfile: {}", type_name_of_error(&e))))?;

        Ok(Self { wallet })
    }

    pub fn to_keyfile<P: AsRef<Path>>(&self, dir: P, password: &str, name: Option<String>) -> Result<(), WalletError> {
        let dir_path = dir.as_ref();

        let secret = self.wallet.signer().to_bytes();

        fs::create_dir_all(dir_path)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to create directory: {}", e)))?;

        let mut rng = thread_rng();

        let secret_slice: &[u8] = &secret;

        let filename = name.as_deref().unwrap_or("keystore");

        let _uuid = eth_keystore::encrypt_key(
            dir_path,
            &mut rng,
            secret_slice,
            password.as_bytes(),
            Some(filename),
        )
        .map_err(|e| WalletError::KeyfileEncrypt(format!("failed to encrypt keyfile: {}", e)))?;

        let keyfile_path = dir_path.join(filename);
        let raw = fs::read_to_string(&keyfile_path)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to read back keyfile: {}", e)))?;
        let mut json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to parse keyfile json: {}", e)))?;

        let addr = format!("{:x}", self.wallet.address());
        json["address"] = serde_json::Value::String(addr);

        let updated = serde_json::to_string_pretty(&json)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to serialize keyfile: {}", e)))?;
        fs::write(&keyfile_path, updated)
            .map_err(|e| WalletError::KeyfileWrite(format!("failed to write keyfile: {}", e)))?;

        Ok(())
    }

    pub fn address(&self) -> String {
        ethers::utils::to_checksum(&self.wallet.address(), None)
    }

    pub fn sign_message(&self, message: &str) -> Result<Signature, WalletError> {
        if message.is_empty() {
            return Err(WalletError::EmptyMessage);
        }

        if message.len() > MAX_MESSAGE_SIZE {
            return Err(WalletError::MessageTooLarge(format!(
                "message exceeds maximum size of {} bytes",
                MAX_MESSAGE_SIZE
            )));
        }

        self.runtime()
            .block_on(self.wallet.sign_message(message))
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))
    }

    pub fn sign_typed_data(&self, typed_data: TypedData) -> Result<Signature, WalletError> {
        self.runtime()
            .block_on(self.wallet.sign_typed_data(&typed_data))
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))
    }

    pub fn sign_transaction(&self, tx: &TransactionRequest) -> Result<Signature, WalletError> {
        tx.validate()?;

        let request: TypedTransaction = tx.to_ethers_request().into();
        self.runtime()
            .block_on(self.wallet.sign_transaction(&request))
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))
    }

    pub fn sign_transaction_bytes(&self, tx: &TransactionRequest) -> Result<Vec<u8>, WalletError> {
        tx.validate()?;

        let typed_tx: TypedTransaction = tx.to_ethers_request().into();
        let signature = self.runtime()
            .block_on(self.wallet.sign_transaction(&typed_tx))
            .map_err(|e| WalletError::Operation(sanitize_error(&e)))?;
        
        Ok(typed_tx.rlp_signed(&signature).to_vec())
    }

    pub fn verify_signature(&self, message: &str, signature: &Signature) -> Result<Address, WalletError> {
        if message.is_empty() {
            return Err(WalletError::EmptyMessage);
        }

        let recovered = signature
            .recover(message)
            .map_err(|error| WalletError::Operation(error.to_string()))?;
        
        Address::new(ethers::utils::to_checksum(&recovered, None)).map_err(WalletError::from)
    }

    fn runtime(&self) -> Runtime {
        Runtime::new().expect("tokio runtime should be available")
    }
}

impl fmt::Debug for WalletManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletManager")
            .field("address", &self.address())
            .finish()
    }
}

impl fmt::Display for WalletManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WalletManager(address={})", self.address())
    }
}

#[derive(Debug, thiserror::Error)]
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
    let msg = error.to_string();
    if msg.len() > 100 {
        format!("{}...", &msg[..100])
    } else {
        msg
    }
}

fn type_name_of_error(_error: &dyn std::error::Error) -> &'static str {
    "key parsing error"
}
