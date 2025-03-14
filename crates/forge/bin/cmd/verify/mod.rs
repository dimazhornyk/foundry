use super::retry::RetryArgs;
use alloy_primitives::Address;
use clap::{Parser, ValueHint};
use eyre::Result;
use foundry_cli::{opts::EtherscanOpts, utils::LoadConfig};
use foundry_compilers::{info::ContractInfo, EvmVersion};
use foundry_config::{figment, impl_figment_convert, impl_figment_convert_cast, Config};
use provider::VerificationProviderType;
use reqwest::Url;
use std::path::PathBuf;

mod etherscan;
use etherscan::EtherscanVerificationProvider;

pub mod provider;
use provider::VerificationProvider;

mod sourcify;

/// Verification provider arguments
#[derive(Clone, Debug, Parser)]
pub struct VerifierArgs {
    /// The contract verification provider to use.
    #[arg(long, help_heading = "Verifier options", default_value = "etherscan", value_enum)]
    pub verifier: VerificationProviderType,

    /// The verifier URL, if using a custom provider
    #[arg(long, help_heading = "Verifier options", env = "VERIFIER_URL")]
    pub verifier_url: Option<String>,
}

impl Default for VerifierArgs {
    fn default() -> Self {
        VerifierArgs { verifier: VerificationProviderType::Etherscan, verifier_url: None }
    }
}

/// CLI arguments for `forge verify`.
#[derive(Clone, Debug, Parser)]
pub struct VerifyArgs {
    /// The address of the contract to verify.
    pub address: Address,

    /// The contract identifier in the form `<path>:<contractname>`.
    pub contract: ContractInfo,

    /// The ABI-encoded constructor arguments.
    #[arg(
        long,
        conflicts_with = "constructor_args_path",
        value_name = "ARGS",
        visible_alias = "encoded-constructor-args"
    )]
    pub constructor_args: Option<String>,

    /// The path to a file containing the constructor arguments.
    #[arg(long, value_hint = ValueHint::FilePath, value_name = "PATH")]
    pub constructor_args_path: Option<PathBuf>,

    /// The `solc` version to use to build the smart contract.
    #[arg(long, value_name = "VERSION")]
    pub compiler_version: Option<String>,

    /// The number of optimization runs used to build the smart contract.
    #[arg(long, visible_alias = "optimizer-runs", value_name = "NUM")]
    pub num_of_optimizations: Option<usize>,

    /// Flatten the source code before verifying.
    #[arg(long)]
    pub flatten: bool,

    /// Do not compile the flattened smart contract before verifying (if --flatten is passed).
    #[arg(short, long)]
    pub force: bool,

    /// Do not check if the contract is already verified before verifying.
    #[arg(long)]
    pub skip_is_verified_check: bool,

    /// Wait for verification result after submission.
    #[arg(long)]
    pub watch: bool,

    /// Set pre-linked libraries.
    #[arg(long, help_heading = "Linker options", env = "DAPP_LIBRARIES")]
    pub libraries: Vec<String>,

    /// The project's root path.
    ///
    /// By default root of the Git repository, if in one,
    /// or the current working directory.
    #[arg(long, value_hint = ValueHint::DirPath, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Prints the standard json compiler input.
    ///
    /// The standard json compiler input can be used to manually submit contract verification in
    /// the browser.
    #[arg(long, conflicts_with = "flatten")]
    pub show_standard_json_input: bool,

    /// Use the Yul intermediate representation compilation pipeline.
    #[arg(long)]
    pub via_ir: bool,

    /// The EVM version to use.
    ///
    /// Overrides the version specified in the config.
    #[arg(long)]
    pub evm_version: Option<EvmVersion>,

    #[command(flatten)]
    pub etherscan: EtherscanOpts,

    #[command(flatten)]
    pub retry: RetryArgs,

    #[command(flatten)]
    pub verifier: VerifierArgs,
}

impl_figment_convert!(VerifyArgs);

impl figment::Provider for VerifyArgs {
    fn metadata(&self) -> figment::Metadata {
        figment::Metadata::named("Verify Provider")
    }

    fn data(
        &self,
    ) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        let mut dict = self.etherscan.dict();
        if let Some(root) = self.root.as_ref() {
            dict.insert("root".to_string(), figment::value::Value::serialize(root)?);
        }
        if let Some(optimizer_runs) = self.num_of_optimizations {
            dict.insert("optimizer".to_string(), figment::value::Value::serialize(true)?);
            dict.insert(
                "optimizer_runs".to_string(),
                figment::value::Value::serialize(optimizer_runs)?,
            );
        }
        if let Some(evm_version) = self.evm_version {
            dict.insert("evm_version".to_string(), figment::value::Value::serialize(evm_version)?);
        }
        if self.via_ir {
            dict.insert("via_ir".to_string(), figment::value::Value::serialize(self.via_ir)?);
        }
        Ok(figment::value::Map::from([(Config::selected_profile(), dict)]))
    }
}

impl VerifyArgs {
    /// Run the verify command to submit the contract's source code for verification on etherscan
    pub async fn run(mut self) -> Result<()> {
        let config = self.load_config_emit_warnings();
        let chain = config.chain.unwrap_or_default();
        self.etherscan.chain = Some(chain);
        self.etherscan.key = config.get_etherscan_config_with_chain(Some(chain))?.map(|c| c.key);

        if self.show_standard_json_input {
            let args =
                EtherscanVerificationProvider::default().create_verify_request(&self, None).await?;
            println!("{}", args.source);
            return Ok(())
        }

        let verifier_url = self.verifier.verifier_url.clone();
        println!("Start verifying contract `{}` deployed on {chain}", self.address);
        self.verifier.verifier.client(&self.etherscan.key())?.verify(self).await.map_err(|err| {
            if let Some(verifier_url) = verifier_url {
                 match Url::parse(&verifier_url) {
                    Ok(url) => {
                        if is_host_only(&url) {
                            return err.wrap_err(format!(
                                "Provided URL `{verifier_url}` is host only.\n Did you mean to use the API endpoint`{verifier_url}/api` ?"
                            ))
                        }
                    }
                    Err(url_err) => {
                        return err.wrap_err(format!(
                            "Invalid URL {verifier_url} provided: {url_err}"
                        ))
                    }
                }
            }

            err
        })
    }

    /// Returns the configured verification provider
    pub fn verification_provider(&self) -> Result<Box<dyn VerificationProvider>> {
        self.verifier.verifier.client(&self.etherscan.key())
    }
}

/// Check verification status arguments
#[derive(Clone, Debug, Parser)]
pub struct VerifyCheckArgs {
    /// The verification ID.
    ///
    /// For Etherscan - Submission GUID.
    ///
    /// For Sourcify - Contract Address.
    id: String,

    #[command(flatten)]
    retry: RetryArgs,

    #[command(flatten)]
    etherscan: EtherscanOpts,

    #[command(flatten)]
    verifier: VerifierArgs,
}

impl_figment_convert_cast!(VerifyCheckArgs);

impl VerifyCheckArgs {
    /// Run the verify command to submit the contract's source code for verification on etherscan
    pub async fn run(self) -> Result<()> {
        println!("Checking verification status on {}", self.etherscan.chain.unwrap_or_default());
        self.verifier.verifier.client(&self.etherscan.key())?.check(self).await
    }
}

impl figment::Provider for VerifyCheckArgs {
    fn metadata(&self) -> figment::Metadata {
        figment::Metadata::named("Verify Check Provider")
    }

    fn data(
        &self,
    ) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        self.etherscan.data()
    }
}

/// Returns `true` if the URL only consists of host.
///
/// This is used to check user input url for missing /api path
#[inline]
fn is_host_only(url: &Url) -> bool {
    matches!(url.path(), "/" | "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_only() {
        assert!(!is_host_only(&Url::parse("https://blockscout.net/api").unwrap()));
        assert!(is_host_only(&Url::parse("https://blockscout.net/").unwrap()));
        assert!(is_host_only(&Url::parse("https://blockscout.net").unwrap()));
    }

    #[test]
    fn can_parse_verify_contract() {
        let args: VerifyArgs = VerifyArgs::parse_from([
            "foundry-cli",
            "0x0000000000000000000000000000000000000000",
            "src/Domains.sol:Domains",
            "--via-ir",
        ]);
        assert!(args.via_ir);
    }
}
