//! Offline-only generation and recovery for woodland.sh mainnet authority keys.
//!
//! Two independent BIP39 roots are split into hardened BIP32 role paths. Root
//! mnemonics never belong on deployment or maintenance hosts.

use anyhow::{anyhow, bail, Context, Result};
use bip39::{Language, Mnemonic};
use bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::{Keypair, Secp256k1};
use bitcoin::Network;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
#[cfg(all(unix, test))]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const SCHEME: &str = "woodland-bip39-bip32-v1";
const DEPLOYER_PATH: &str = "m/1464815428'/1'/0'/0'";
const MAINTENANCE_PATH: &str = "m/1464815428'/1'/0'/1'";
const ROLLOVER_PATH: &str = "m/1464815428'/1'/0'/2'";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicKeyRecord {
    derivation_path: String,
    xonly_public_key: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicBundle {
    scheme: String,
    network: String,
    deployment_root_fingerprint: String,
    operations_root_fingerprint: String,
    deployer: PublicKeyRecord,
    maintenance: PublicKeyRecord,
    rollover: PublicKeyRecord,
}

struct DerivedBundle {
    public: PublicBundle,
    deployer_secret: String,
    maintenance_secret: String,
    rollover_secret: String,
}

fn generate_mnemonic() -> Result<Mnemonic> {
    let mut entropy = [0_u8; 32];
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy)
        .context("generate 24-word BIP39 mnemonic")?;
    entropy.fill(0);
    Ok(mnemonic)
}

fn root_from_mnemonic(mnemonic: &Mnemonic) -> Result<Xpriv> {
    let mut seed = mnemonic.to_seed_normalized("");
    let root = Xpriv::new_master(Network::Bitcoin, &seed).context("derive BIP32 root")?;
    seed.fill(0);
    Ok(root)
}

fn derive_child(root: &Xpriv, path: &str) -> Result<(String, String)> {
    let secp = Secp256k1::new();
    let path = DerivationPath::from_str(path).context("parse hardened derivation path")?;
    let child = root
        .derive_priv(&secp, &path)
        .context("derive hardened child key")?;
    let keypair = Keypair::from_secret_key(&secp, &child.private_key);
    let xonly = keypair.x_only_public_key().0;
    Ok((
        child.private_key.secret_bytes().to_lower_hex_string(),
        xonly.to_string(),
    ))
}

fn derive_bundle(deployment: &Mnemonic, operations: &Mnemonic) -> Result<DerivedBundle> {
    if deployment == operations {
        bail!("deployment and operations roots must be independent");
    }
    let secp = Secp256k1::new();
    let deployment_root = root_from_mnemonic(deployment)?;
    let operations_root = root_from_mnemonic(operations)?;
    let deployment_fingerprint = Xpub::from_priv(&secp, &deployment_root)
        .fingerprint()
        .to_string();
    let operations_fingerprint = Xpub::from_priv(&secp, &operations_root)
        .fingerprint()
        .to_string();
    let (deployer_secret, deployer_public) = derive_child(&deployment_root, DEPLOYER_PATH)?;
    let (maintenance_secret, maintenance_public) =
        derive_child(&operations_root, MAINTENANCE_PATH)?;
    let (rollover_secret, rollover_public) = derive_child(&operations_root, ROLLOVER_PATH)?;

    Ok(DerivedBundle {
        public: PublicBundle {
            scheme: SCHEME.to_owned(),
            network: "bitcoin".to_owned(),
            deployment_root_fingerprint: deployment_fingerprint,
            operations_root_fingerprint: operations_fingerprint,
            deployer: PublicKeyRecord {
                derivation_path: DEPLOYER_PATH.to_owned(),
                xonly_public_key: deployer_public,
            },
            maintenance: PublicKeyRecord {
                derivation_path: MAINTENANCE_PATH.to_owned(),
                xonly_public_key: maintenance_public,
            },
            rollover: PublicKeyRecord {
                derivation_path: ROLLOVER_PATH.to_owned(),
                xonly_public_key: rollover_public,
            },
        },
        deployer_secret,
        maintenance_secret,
        rollover_secret,
    })
}

fn create_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        bail!(
            "refusing to overwrite existing output path {}",
            path.display()
        );
    }
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("create private directory {}", path.display()))
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("create private file {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write private file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync private file {}", path.display()))
}

fn write_derived_files(output: &Path, bundle: &DerivedBundle) -> Result<()> {
    write_private(
        &output.join("deployment.env"),
        &format!("WOODLAND_DEPLOYER_SECRET={}\n", bundle.deployer_secret),
    )?;
    write_private(
        &output.join("maintenance.env"),
        &format!(
            "WOODLAND_TREE_MAINTENANCE_SECRET={}\nWOODLAND_ROLLOVER_SECRET={}\n",
            bundle.maintenance_secret, bundle.rollover_secret
        ),
    )?;
    write_private(
        &output.join("public.json"),
        &format!("{}\n", serde_json::to_string_pretty(&bundle.public)?),
    )?;
    write_private(
        &output.join("README.txt"),
        &format!(
            "woodland.sh mainnet key bundle\n\nScheme: {SCHEME}\nDeployment root fingerprint: {}\nOperations root fingerprint: {}\n\nKeep root mnemonic files on offline backup media and never copy them to a maintenance host. Copy only maintenance.env to that host. Remove deployment.env from online systems after world deployment and balance verification.\n",
            bundle.public.deployment_root_fingerprint, bundle.public.operations_root_fingerprint
        ),
    )
}

fn read_mnemonic(path: &Path) -> Result<Mnemonic> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read mnemonic file {}", path.display()))?;
    Mnemonic::parse_in(Language::English, text.trim())
        .with_context(|| format!("parse mnemonic file {}", path.display()))
}

fn print_public(bundle: &PublicBundle) {
    println!(
        "deployment root fingerprint: {}",
        bundle.deployment_root_fingerprint
    );
    println!(
        "operations root fingerprint: {}",
        bundle.operations_root_fingerprint
    );
    println!(
        "deployer {}: {}",
        bundle.deployer.derivation_path, bundle.deployer.xonly_public_key
    );
    println!(
        "maintenance {}: {}",
        bundle.maintenance.derivation_path, bundle.maintenance.xonly_public_key
    );
    println!(
        "rollover {}: {}",
        bundle.rollover.derivation_path, bundle.rollover.xonly_public_key
    );
}

fn generate(output: PathBuf) -> Result<()> {
    create_private_directory(&output)?;
    let deployment = generate_mnemonic()?;
    let operations = loop {
        let candidate = generate_mnemonic()?;
        if candidate != deployment {
            break candidate;
        }
    };
    let bundle = derive_bundle(&deployment, &operations)?;
    write_private(
        &output.join("deployment-root.txt"),
        &format!("{deployment}\n"),
    )?;
    write_private(
        &output.join("operations-root.txt"),
        &format!("{operations}\n"),
    )?;
    write_derived_files(&output, &bundle)?;
    print_public(&bundle.public);
    eprintln!(
        "generated plaintext recovery material in {}",
        output.display()
    );
    eprintln!("move the two root files offline before using any online environment file");
    Ok(())
}

fn recover(deployment_path: PathBuf, operations_path: PathBuf, output: PathBuf) -> Result<()> {
    let deployment = read_mnemonic(&deployment_path)?;
    let operations = read_mnemonic(&operations_path)?;
    let bundle = derive_bundle(&deployment, &operations)?;
    create_private_directory(&output)?;
    write_derived_files(&output, &bundle)?;
    print_public(&bundle.public);
    Ok(())
}

fn verify(deployment_path: PathBuf, operations_path: PathBuf, public_path: PathBuf) -> Result<()> {
    let deployment = read_mnemonic(&deployment_path)?;
    let operations = read_mnemonic(&operations_path)?;
    let actual = derive_bundle(&deployment, &operations)?.public;
    let expected: PublicBundle = serde_json::from_str(
        &fs::read_to_string(&public_path)
            .with_context(|| format!("read public key file {}", public_path.display()))?,
    )
    .with_context(|| format!("parse public key file {}", public_path.display()))?;
    if actual != expected {
        bail!("recovered roots do not match {}", public_path.display());
    }
    print_public(&actual);
    println!("recovery verified");
    Ok(())
}

#[cfg(unix)]
fn require_private_permissions() -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn require_private_permissions() -> Result<()> {
    bail!("woodland-keygen requires Unix 0600/0700 permission semantics")
}

fn usage() -> anyhow::Error {
    anyhow!(
        "usage:\n  woodland-keygen generate <new-output-directory>\n  woodland-keygen recover <deployment-root.txt> <operations-root.txt> <new-output-directory>\n  woodland-keygen verify <deployment-root.txt> <operations-root.txt> <public.json>"
    )
}

fn run() -> Result<()> {
    require_private_permissions()?;
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("generate") => {
            let output = args.next().map(PathBuf::from).ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            generate(output)
        }
        Some("recover") => {
            let deployment = args.next().map(PathBuf::from).ok_or_else(usage)?;
            let operations = args.next().map(PathBuf::from).ok_or_else(usage)?;
            let output = args.next().map(PathBuf::from).ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            recover(deployment, operations, output)
        }
        Some("verify") => {
            let deployment = args.next().map(PathBuf::from).ok_or_else(usage)?;
            let operations = args.next().map(PathBuf::from).ok_or_else(usage)?;
            let public = args.next().map(PathBuf::from).ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            verify(deployment, operations, public)
        }
        _ => Err(usage()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Public BIP39 test vectors; these words never protect deployed keys.
    const TEST_DEPLOYMENT_MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const TEST_OPERATIONS_MNEMONIC: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    fn test_mnemonic(words: &str) -> Mnemonic {
        Mnemonic::parse_in(Language::English, words).unwrap()
    }

    #[test]
    fn hardened_role_derivation_is_deterministic_and_distinct() {
        let deployment = test_mnemonic(TEST_DEPLOYMENT_MNEMONIC);
        let operations = test_mnemonic(TEST_OPERATIONS_MNEMONIC);
        let first = derive_bundle(&deployment, &operations).unwrap();
        let second = derive_bundle(&deployment, &operations).unwrap();
        assert_eq!(first.public, second.public);
        assert_eq!(first.public.deployment_root_fingerprint, "73c5da0a");
        assert_eq!(first.public.operations_root_fingerprint, "b8688df1");
        assert_eq!(
            first.public.deployer.xonly_public_key,
            "a0990f658bc1ebee6f0bc63fb840035a1e8dc1f7fc3b7007dec349b80fb85e97"
        );
        assert_eq!(
            first.public.maintenance.xonly_public_key,
            "215fc9f246b711433725d1a662dc03845bbf6e85e2407131db5f8468f2d713a5"
        );
        assert_eq!(
            first.public.rollover.xonly_public_key,
            "25f24bed1d799a7bae60b17c50a3e7bd05f07239c4584b809415825895926079"
        );
        assert_ne!(first.deployer_secret, first.maintenance_secret);
        assert_ne!(first.maintenance_secret, first.rollover_secret);
        assert_eq!(first.public.deployer.derivation_path, DEPLOYER_PATH);
        assert_eq!(first.public.maintenance.derivation_path, MAINTENANCE_PATH);
        assert_eq!(first.public.rollover.derivation_path, ROLLOVER_PATH);
    }

    #[cfg(unix)]
    #[test]
    fn generated_bundle_uses_private_permissions_and_recovers() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("woodland-keygen-{}-{unique}", std::process::id()));
        let generated = root.join("generated");
        let recovered = root.join("recovered");
        create_private_directory(&generated).unwrap();
        let deployment = test_mnemonic(TEST_DEPLOYMENT_MNEMONIC);
        let operations = test_mnemonic(TEST_OPERATIONS_MNEMONIC);
        let bundle = derive_bundle(&deployment, &operations).unwrap();
        write_private(
            &generated.join("deployment-root.txt"),
            &format!("{deployment}\n"),
        )
        .unwrap();
        write_private(
            &generated.join("operations-root.txt"),
            &format!("{operations}\n"),
        )
        .unwrap();
        write_derived_files(&generated, &bundle).unwrap();

        assert_eq!(
            fs::metadata(&generated).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in [
            "deployment-root.txt",
            "operations-root.txt",
            "deployment.env",
            "maintenance.env",
            "public.json",
            "README.txt",
        ] {
            assert_eq!(
                fs::metadata(generated.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{name}",
            );
        }

        recover(
            generated.join("deployment-root.txt"),
            generated.join("operations-root.txt"),
            recovered.clone(),
        )
        .unwrap();
        verify(
            generated.join("deployment-root.txt"),
            generated.join("operations-root.txt"),
            recovered.join("public.json"),
        )
        .unwrap();
        assert!(!recovered.join("deployment-root.txt").exists());
        assert!(!recovered.join("operations-root.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
