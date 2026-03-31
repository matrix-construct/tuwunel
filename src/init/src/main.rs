use anyhow::{Context, Result};
use clap::Parser;
use inquire::{Text, Password, PasswordDisplayMode, validator::Validation};
use regex::Regex;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
#[command(name = "tuwunel-init")]
#[command(about = "Initialize tuwunel configuration and create the first user")]
struct Cli {
    #[arg(short, long)]
    path: Option<PathBuf>,
}

fn estimate_entropy(password: &str) -> f64 {
    let mut pool = 0;
    if password.chars().any(|c| c.is_ascii_lowercase()) { pool += 26; }
    if password.chars().any(|c| c.is_ascii_uppercase()) { pool += 26; }
    if password.chars().any(|c| c.is_ascii_digit()) { pool += 10; }
    if password.chars().any(|c| !c.is_ascii_alphanumeric()) { pool += 32; }
    
    if pool == 0 { return 0.0; }
    (pool as f64).log2() * (password.chars().count() as f64)
}

fn classify_entropy(entropy: f64) -> &'static str {
    if entropy < 40.0 { "Poor" }
    else if entropy < 60.0 { "Ok" }
    else if entropy < 80.0 { "Good" }
    else { "Excellent" }
}

fn main() -> Result<()> {
    let args = Cli::parse();
    
    let config_path = args.path.or_else(|| {
        env::var("TUWUNEL_CONFIG").ok().map(PathBuf::from)
    }).unwrap_or_else(|| PathBuf::from("tuwunel.toml"));
    
    if config_path.exists() {
        println!("Configuration file already exists at {}. Exiting gracefully.", config_path.display());
        return Ok(());
    }
    
    if let Some(parent) = config_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).context("Failed to create parent directories")?;
        }
    }

    println!("Tuwunel initialization.");
    println!("We will create a minimal configuration file for your matrix server.\n");
    
    let domain_regex = Regex::new(r"^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(\.[a-zA-Z]{2,})+$").unwrap();

    let server_name = Text::new("Server name (e.g., matrix.example.com):")
        .with_help_message("This must be identical to the domain used for reverse proxying.")
        .with_validator(move |input: &str| {
            if input.trim().is_empty() {
                Ok(Validation::Invalid("Server name cannot be empty.".into()))
            } else if !domain_regex.is_match(input.trim()) {
                Ok(Validation::Invalid("Please enter a valid fully qualified domain name.".into()))
            } else {
                Ok(Validation::Valid)
            }
        })
        .prompt()
        .context("Failed to read server name")?;
        
    let server_name = server_name.trim();

    let registration_token = Password::new("Registration token:")
        .with_display_mode(PasswordDisplayMode::Masked)
        .with_help_message("A static registration token users will have to provide when creating an account.")
        .with_validator(move |input: &str| {
            if input.trim().is_empty() {
                return Ok(Validation::Invalid("Registration token cannot be empty.".into()));
            }
            let ent = estimate_entropy(input.trim());
            if ent < 40.0 {
                Ok(Validation::Invalid("Registration is insecure. Please choose a more secure token.".into()))
            } else {
                Ok(Validation::Valid)
            }
        })
        .prompt()
        .context("Failed to read registration token")?;
        
    let registration_token = registration_token.trim();
    println!("Registration token entropy: {}", classify_entropy(estimate_entropy(registration_token)));

    let create_user_decision = Text::new("Would you like to create an initial admin user? [Yes/No/Quit]:")
        .with_validator(|input: &str| {
            let s = input.trim().to_lowercase();
            match s.as_str() {
                "y" | "yes" | "n" | "no" | "q" | "quit" | "" => Ok(Validation::Valid),
                _ => Ok(Validation::Invalid("Please enter yes, no, or quit.".into()))
            }
        })
        .prompt()
        .context("Failed to read user creation decision")?;
        
    let decision = create_user_decision.trim().to_lowercase();
    if decision == "q" || decision == "quit" {
        println!("Exiting without writing configuration.");
        return Ok(());
    }

    let create_user = decision == "y" || decision == "yes" || decision == "";

    let mut admin_username_opt = None;
    let mut admin_password_opt = None;

    if create_user {
        let admin_username = Text::new("Admin username (without domain portion, eg. 'tom'):")
            .with_validator(|input: &str| {
                if input.trim().is_empty() {
                    Ok(Validation::Invalid("Admin username cannot be empty.".into()))
                } else if input.contains(':') || input.contains('@') {
                    Ok(Validation::Invalid("Just the local username, no @ or domain.".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()
            .context("Failed to read admin username")?;
            
        let admin_username = admin_username.trim().to_string();

        let admin_password = Password::new("Admin password:")
            .with_display_mode(PasswordDisplayMode::Masked)
            .with_validator(move |input: &str| {
                if input.trim().is_empty() {
                    return Ok(Validation::Invalid("Admin password cannot be empty.".into()));
                }
                let ent = estimate_entropy(input.trim());
                if ent < 40.0 {
                    Ok(Validation::Invalid(format!("Admin password entropy is {}. Please choose a stronger password.", classify_entropy(ent)).into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()
            .context("Failed to read admin password")?;
            
        let admin_password = admin_password.trim().to_string();
        println!("Admin password security: {}", classify_entropy(estimate_entropy(&admin_password)));

        admin_username_opt = Some(admin_username);
        admin_password_opt = Some(admin_password);
    }

    // Read the build-time generated config example (this embeds the file directly into the binary at compile time!)
    let mut config_text = include_str!("../../../tuwunel-example.toml").to_string();
    
    config_text = config_text.replace(
        "#server_name =\n",
        &format!("server_name = \"{}\"\n", server_name)
    );
    config_text = config_text.replace(
        "#registration_token =\n",
        &format!("registration_token = \"{}\"\n", registration_token)
    );

    // If running inside a Snap, we must change the default database path to the writable SNAP_COMMON directory
    if let Ok(snap_common) = env::var("SNAP_COMMON") {
        config_text = config_text.replace(
            "#database_path = \"/var/lib/tuwunel\"",
            &format!("database_path = \"{snap_common}/db\"")
        );
    }

    fs::write(&config_path, config_text).context("Failed to write tuwunel config file")?;
    println!("\nConfiguration file successfully written to {}", config_path.display());

    if !create_user {
        println!("\nInitialization Complete! \u{1F389}");
        println!("The daemon can now be started. If using snap, the service will pick up the config momentarily.");
        return Ok(());
    }
    
    // Attempt to invoke the Daemon binary to create the admin user
    // We expect `tuwunel` binary to be situated in the exact same directory as this tool.
    let mut tuwunel_bin = PathBuf::from("tuwunel");
    if let Ok(current_exe) = env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let adjacent = parent.join("tuwunel");
            if adjacent.exists() {
                tuwunel_bin = adjacent;
            }
        }
    }

    // Pass the config path directly to tuwunel as --config
    println!("Initializing database and to create the first administrator...");
    let admin_username = admin_username_opt.unwrap();
    let admin_password = admin_password_opt.unwrap();
    let status = Command::new(&tuwunel_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--maintenance")
        .arg("--execute")
        .arg(format!("users create_user @{}:{} {}", admin_username, server_name, admin_password))
        .arg("--execute")
        .arg("server shutdown")
        .status()
        .with_context(|| format!("Failed to evaluate tuwunel binary: {}", tuwunel_bin.display()))?;

    if status.success() {
        println!("\nInitialization Complete! \u{1F389}");
        println!("The daemon can now be started, or will start automatically if you installed the Tuwunel snap.");
        println!("It will by default listen only on 127.0.0.1:8008, you will probably want to direct traffic");
        println!("there by reverse proxy. See the docs for more information.");
        println!("");
        println!("You can sign into the server at:");
        println!("");
        println!("  Homeserver URL: https://{server_name}");
        println!("  Username: @{admin_username}:{server_name}");
        println!("  Password: <the password you provided>");
    } else {
        println!("\nUser creation failed unexpectedly (exited with: {status}).");
        println!("You can attempt to retry creating the admin manually by running:");
        println!("  {} --maintenance --execute 'user create ...'", tuwunel_bin.display());
    }
    
    Ok(())
}
