use std::collections::HashSet;
use std::fs::File;
use std::time::Duration;

use anyhow::{bail, format_err, Error};
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};

use proxmox::api::api;
use proxmox::api::schema::{self, Updater};
use proxmox::tools::fs::{replace_file, CreateOptions};

use crate::acme::AcmeClient;
use crate::config::acme::{AccountName, AcmeDomain};

const CONF_FILE: &str = configdir!("/node.cfg");
const LOCK_FILE: &str = configdir!("/.node.cfg.lck");
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn lock() -> Result<File, Error> {
    proxmox::tools::fs::open_file_locked(LOCK_FILE, LOCK_TIMEOUT, true)
}

/// Read the Node Config.
pub fn config() -> Result<(NodeConfig, [u8; 32]), Error> {
    let content =
        proxmox::tools::fs::file_read_optional_string(CONF_FILE)?.unwrap_or_else(|| "".to_string());

    let digest = openssl::sha::sha256(content.as_bytes());
    let data: NodeConfig = crate::tools::config::from_str(&content, &NodeConfig::API_SCHEMA)?;

    Ok((data, digest))
}

/// Write the Node Config, requires the write lock to be held.
pub fn save_config(config: &NodeConfig) -> Result<(), Error> {
    config.validate()?;

    let raw = crate::tools::config::to_bytes(config, &NodeConfig::API_SCHEMA)?;

    let backup_user = crate::backup::backup_user()?;
    let options = CreateOptions::new()
        .perm(Mode::from_bits_truncate(0o0640))
        .owner(nix::unistd::ROOT)
        .group(backup_user.gid);

    replace_file(CONF_FILE, &raw, options)
}

#[api(
    properties: {
        account: { type: AccountName },
    }
)]
#[derive(Deserialize, Serialize)]
/// The ACME configuration.
///
/// Currently only contains the name of the account use.
pub struct AcmeConfig {
    /// Account to use to acquire ACME certificates.
    account: AccountName,
}

#[api(
    properties: {
        acme: {
            optional: true,
            type: String,
            format: &schema::ApiStringFormat::PropertyString(&AcmeConfig::API_SCHEMA),
        },
        acmedomain0: {
            type: String,
            optional: true,
            format: &schema::ApiStringFormat::PropertyString(&AcmeDomain::API_SCHEMA),
        },
        acmedomain1: {
            type: String,
            optional: true,
            format: &schema::ApiStringFormat::PropertyString(&AcmeDomain::API_SCHEMA),
        },
        acmedomain2: {
            type: String,
            optional: true,
            format: &schema::ApiStringFormat::PropertyString(&AcmeDomain::API_SCHEMA),
        },
        acmedomain3: {
            type: String,
            optional: true,
            format: &schema::ApiStringFormat::PropertyString(&AcmeDomain::API_SCHEMA),
        },
        acmedomain4: {
            type: String,
            optional: true,
            format: &schema::ApiStringFormat::PropertyString(&AcmeDomain::API_SCHEMA),
        },
    },
)]
#[derive(Deserialize, Serialize, Updater)]
/// Node specific configuration.
pub struct NodeConfig {
    /// The acme account to use on this node.
    #[serde(skip_serializing_if = "Updater::is_empty")]
    acme: Option<String>,

    /// ACME domain to get a certificate for for this node.
    #[serde(skip_serializing_if = "Updater::is_empty")]
    acmedomain0: Option<String>,

    /// ACME domain to get a certificate for for this node.
    #[serde(skip_serializing_if = "Updater::is_empty")]
    acmedomain1: Option<String>,

    /// ACME domain to get a certificate for for this node.
    #[serde(skip_serializing_if = "Updater::is_empty")]
    acmedomain2: Option<String>,

    /// ACME domain to get a certificate for for this node.
    #[serde(skip_serializing_if = "Updater::is_empty")]
    acmedomain3: Option<String>,

    /// ACME domain to get a certificate for for this node.
    #[serde(skip_serializing_if = "Updater::is_empty")]
    acmedomain4: Option<String>,
}

impl NodeConfig {
    pub fn acme_config(&self) -> Option<Result<AcmeConfig, Error>> {
        self.acme.as_deref().map(|config| -> Result<_, Error> {
            Ok(crate::tools::config::from_property_string(
                config,
                &AcmeConfig::API_SCHEMA,
            )?)
        })
    }

    pub async fn acme_client(&self) -> Result<AcmeClient, Error> {
        AcmeClient::load(
            &self
                .acme_config()
                .ok_or_else(|| format_err!("no acme client configured"))??
                .account,
        )
        .await
    }

    pub fn acme_domains(&self) -> AcmeDomainIter {
        AcmeDomainIter::new(self)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), Error> {
        let mut domains = HashSet::new();
        for domain in self.acme_domains() {
            let domain = domain?;
            if !domains.insert(domain.domain.to_lowercase()) {
                bail!("duplicate domain '{}' in ACME config", domain.domain);
            }
        }

        Ok(())
    }
}

pub struct AcmeDomainIter<'a> {
    config: &'a NodeConfig,
    index: usize,
}

impl<'a> AcmeDomainIter<'a> {
    fn new(config: &'a NodeConfig) -> Self {
        Self { config, index: 0 }
    }
}

impl<'a> Iterator for AcmeDomainIter<'a> {
    type Item = Result<AcmeDomain, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let domain = loop {
            let index = self.index;
            self.index += 1;

            let domain = match index {
                0 => self.config.acmedomain0.as_deref(),
                1 => self.config.acmedomain1.as_deref(),
                2 => self.config.acmedomain2.as_deref(),
                3 => self.config.acmedomain3.as_deref(),
                4 => self.config.acmedomain4.as_deref(),
                _ => return None,
            };

            if let Some(domain) = domain {
                break domain;
            }
        };

        Some(crate::tools::config::from_property_string(
            domain,
            &AcmeDomain::API_SCHEMA,
        ))
    }
}
