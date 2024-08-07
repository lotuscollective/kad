use crate::util::{decode_hex, timestamp, Addr, Hash, RpcArgs, RpcContext, RpcOp, RpcResult, RpcResults};
use futures::Future;
use rsa::pkcs1::{
    self, DecodeRsaPrivateKey, DecodeRsaPublicKey, EncodeRsaPrivateKey, EncodeRsaPublicKey,
};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::Sha256;
use rsa::signature::{RandomizedSigner, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use tokio::sync::RwLock;

pub(crate) const KEY_BITS: usize = 2048;
pub(crate) const MAX_KEYS: usize = 64;

pub(crate) struct Crypto {
    pub(crate) private: RsaPrivateKey,
    pub(crate) public: RsaPublicKey,
    pub(crate) signing: SigningKey<Sha256>,

    // hash -> (key, last contacted (for pruning))
    pub(crate) keyring: RwLock<BTreeMap<Hash, (RsaPublicKey, u64)>>,
}

impl Crypto {
    // randomly generate key
    pub(crate) fn new() -> Result<Self, Box<dyn Error>> {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, KEY_BITS)?;
        let public_key = RsaPublicKey::from(private_key.clone());

        Ok(Crypto {
            private: private_key.clone(),
            public: public_key,
            signing: SigningKey::<Sha256>::new(private_key),
            keyring: RwLock::new(BTreeMap::new()),
        })
    }

    pub(crate) fn from_file(priv_file: &str, pub_file: &str) -> Result<Self, Box<dyn Error>> {
        let (priv_path, pub_path) = (Path::new(priv_file), Path::new(pub_file));
        let (private_key, public_key) = (
            RsaPrivateKey::read_pkcs1_pem_file(priv_path)?,
            RsaPublicKey::read_public_key_pem_file(pub_path)?,
        );

        Ok(Crypto {
            private: private_key.clone(),
            public: public_key,
            signing: SigningKey::<Sha256>::new(private_key),
            keyring: RwLock::new(BTreeMap::new()),
        })
    }

    pub(crate) fn public_key_as_string(&self) -> Result<String, Box<dyn Error>> {
        Ok(self.public.to_pkcs1_pem(pkcs1::LineEnding::LF)?)
    }

    pub(crate) fn to_file(&self, priv_file: &str, pub_file: &str) -> Result<(), Box<dyn Error>> {
        self.private
            .write_pkcs1_pem_file(Path::new(priv_file), pkcs1::LineEnding::LF)?;
        self.public
            .write_pkcs1_pem_file(Path::new(pub_file), pkcs1::LineEnding::LF)?;

        Ok(())
    }

    pub(crate) fn sign(&self, data: &str) -> String {
        let mut rng = rand::thread_rng();

        self.signing
            .sign_with_rng(&mut rng, data.as_bytes())
            .to_string()
    }

    // verify with existing key
    pub(crate) async fn verify(&self, id: Hash, data: &str, sig: &str) -> bool {
        let keyring = self.keyring.read().await;

        let entry = keyring.get(&id).unwrap();
        let ver_key = VerifyingKey::<Sha256>::new(entry.0.clone());

        if let Ok(s) = decode_hex(sig) {
            match Signature::try_from(s.as_slice()) {
                Ok(signature) => {
                    ver_key.verify(data.as_bytes(), &signature).is_ok()
                },
                Err(_) => false,
            }
        } else {
            false
        }
    }

    pub(crate) fn args(&self, i: Hash, o: RpcOp, a: Addr, ts: u64) -> RpcArgs {
        let ctx = RpcContext {
            id: i,
            op: o,
            addr: a,
            timestamp: ts,
        };

        let sign = self.sign(serde_json::to_string(&ctx).unwrap().as_str());

        (ctx, sign)
    }

    pub(crate) fn results(&self, res: RpcResult) -> RpcResults {
        let sign = self.sign(serde_json::to_string(&res).unwrap().as_str());

        (res, sign)
    }

    // add/update key to keyring
    pub(crate) async fn entry(&self, id: Hash, key: &str) -> bool {
        if let Ok(pub_key) = RsaPublicKey::from_pkcs1_pem(key) {  
            let mut keyring = self.keyring.write().await;
            keyring.insert(id, (pub_key, timestamp()));

            if keyring.len() > MAX_KEYS {
                // prune oldest key
                
                let t = keyring
                    .iter_mut()
                    .min_by(|&(_, &mut (_, a)), &(_, &mut (_, b))| a.cmp(&b))
                    .unwrap()
                    .0
                    .to_owned();

                keyring.remove(&t);
            }

            true
        } else {
            false
        }
    }

    // remove from keyring
    pub(crate) async fn remove(&self, id: Hash) {
        let mut keyring = self.keyring.write().await;
        keyring.remove(&id);
    }

    pub(crate) async fn if_unknown<F>(&self, id: Hash, f: impl FnOnce() -> F) -> bool
    where 
        F: Future<Output = bool> {
        let keyring = self.keyring.read().await;

        if !keyring.contains_key(&id) {
            drop(keyring);
            f().await
        } else {
            false
        }
    }

    pub(crate) async fn verify_args<F>(&self, args: &RpcArgs, backup: impl FnOnce() -> F) -> bool
    where
        F: Future<Output = ()>,
    {
        let ctx = args.0.clone();
        let keyring = self.keyring.read().await;

        if keyring.contains_key(&ctx.id) {
            self.verify(
                args.0.id,
                serde_json::to_string(&ctx).as_ref().unwrap(),
                &args.1,
            ).await
        } else {
            // if key doesn't exist, try and get it. if it still doesn't exist, give up.
            drop(keyring);
            backup().await;

            let keyring = self.keyring.read().await;
            if keyring.contains_key(&ctx.id) {                
                self.verify(
                    args.0.id,
                    serde_json::to_string(&ctx).as_ref().unwrap(),
                    &args.1,
                ).await
            } else {
                self.remove(ctx.id).await;
                false
            }
        }
    }

    pub(crate) async fn verify_results(&self, id: Hash, results: &RpcResults) -> bool {
        let keyring = self.keyring.read().await;

        if keyring.contains_key(&id) {
            self.verify(
                id,
                serde_json::to_string(&results.0).as_ref().unwrap(),
                &results.1,
            ).await
        } else {
            self.remove(id).await;
            false
        }
    }
}