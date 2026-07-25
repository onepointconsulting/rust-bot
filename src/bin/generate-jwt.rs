use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rust_bot::api::user_registry::{hash_password, JsonUserRegistry, User, UserRegistry};
use rust_bot::config::loader::{load_config, save_config};
use rust_bot::security::jwt::{
    generate_jwt_keypair, generate_jwt_token, DEFAULT_EXPIRES_IN_MONTHS,
};
use rust_bot::utils::exit_codes::{self, GENERAL_ERROR};

#[derive(Debug, Parser)]
#[command(name = "generate-jwt", about = "Generate Ed25519 JWT keypairs and tokens")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate an Ed25519 keypair and update api.jwt key paths in the config file
    GenerateJwtKeypair {
        /// Path to the rust-bot JSON configuration file
        #[arg(long)]
        config: PathBuf,

        /// Directory where private_key.pem and public_key.pem are written
        #[arg(long, default_value = "./.rust-bot/credentials")]
        credentials_dir: PathBuf,

        /// Overwrite existing key files
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Mint an EdDSA JWT using the private key path from the config file
    GenerateJwtToken {
        /// Path to the rust-bot JSON configuration file
        #[arg(long)]
        config: PathBuf,

        /// Override issuer (defaults to api.jwt.iss from config)
        #[arg(long)]
        iss: Option<String>,

        /// Override audience (defaults to api.jwt.aud from config; empty omits claim)
        #[arg(long)]
        aud: Option<String>,

        /// Token lifetime in months (default: 6)
        #[arg(long, default_value_t = DEFAULT_EXPIRES_IN_MONTHS)]
        expires_in_months: u32,

        /// The email of the user for whom the token is being generated
        #[arg(long, required = true)]
        user_email: String,

        /// Optional password; stored as an Argon2id hash in the users file
        #[arg(long)]
        password: Option<String>,

        /// Path to the JSON user registry file (email -> token map)
        #[arg(long, required = true)]
        users_file: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::GenerateJwtKeypair {
            config,
            credentials_dir,
            force,
        } => {
            if let Err(err) = run_generate_keypair(config, credentials_dir, force) {
                eprintln!("ERROR: {err}");
                exit_codes::exit(GENERAL_ERROR);
            }
        }
        Commands::GenerateJwtToken {
            config,
            iss,
            aud,
            expires_in_months,
            user_email,
            password,
            users_file,
        } => {
            if let Err(err) = run_generate_token(
                config,
                iss,
                aud,
                expires_in_months,
                user_email,
                password,
                users_file,
            ) {
                eprintln!("ERROR: {err}");
                exit_codes::exit(GENERAL_ERROR);
            }
        }
    }
    ExitCode::SUCCESS
}

fn path_for_config(path: PathBuf) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or(path);
    #[cfg(windows)]
    {
        let as_str = canonical.to_string_lossy();
        if let Some(stripped) = as_str.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    canonical
}

fn run_generate_keypair(
    config_path: PathBuf,
    credentials_dir: PathBuf,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = load_config(Some(config_path.clone()));
    let keys = generate_jwt_keypair(&credentials_dir, force)?;

    let private_key_path = path_for_config(keys.private_key_path);
    let public_key_path = path_for_config(keys.public_key_path);

    config.api.jwt.private_key_path = private_key_path.display().to_string();
    config.api.jwt.public_key_path = public_key_path.display().to_string();
    save_config(&config, Some(config_path.clone()))?;

    eprintln!("Wrote private key: {}", private_key_path.display());
    eprintln!("Wrote public key:  {}", public_key_path.display());
    eprintln!(
        "Updated api.jwt key paths in {}",
        config_path.display()
    );
    Ok(())
}

fn run_generate_token(
    config_path: PathBuf,
    iss_override: Option<String>,
    aud_override: Option<String>,
    expires_in_months: u32,
    user_email: String,
    password: Option<String>,
    users_file: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = load_config(Some(config_path.clone()));
    let jwt = &config.api.jwt;

    let iss = iss_override.unwrap_or_else(|| jwt.iss.clone());
    let aud = aud_override.unwrap_or_else(|| jwt.aud.clone());

    let minted = generate_jwt_token(&jwt.private_key_path, iss, aud, expires_in_months)?;

    let mut registry = JsonUserRegistry::open(users_file.clone())?;
    registry.register_user(&User {
        email: user_email,
        password_hash: hash_password(password)?,
        token: minted.token.clone(),
    })?;
    // Canonicalize after register_user so the file exists on disk.
    config.api.users_file = path_for_config(users_file).display().to_string();
    save_config(&config, Some(config_path.clone()))?;

    eprintln!("sub: {}", minted.claims.sub);
    eprintln!(
        "exp: {} (unix)",
        minted.claims.exp
    );
    if let Some(aud) = &minted.claims.aud {
        eprintln!("aud: {aud}");
    } else {
        eprintln!("aud: (omitted)");
    }
    println!("{}", minted.token);
    Ok(())
}
