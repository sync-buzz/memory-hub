//! CLI handlers for installation registry and uninstall commands.

use crate::cli::{Output, RegistryCommand};
use crate::exit::Code;
use crate::registry::{
    InstallationRecord, InstallationRegistry, current_binary_path, file_checksum, now_iso,
};

/// Handle a `memory-hub registry` subcommand.
pub fn handle(subcommand: RegistryCommand, output: Output) -> Code {
    match subcommand {
        RegistryCommand::List => list(output),
        RegistryCommand::RegisterConsumer { name, major } => {
            register_consumer(&name, major, output)
        }
        RegistryCommand::UnregisterConsumer { name } => unregister_consumer(&name, output),
        RegistryCommand::AddRepository { path } => add_repository(&path, output),
        RegistryCommand::RemoveRepository { path } => remove_repository(&path, output),
    }
}

/// Handle `memory-hub uninstall`.
pub fn handle_uninstall(purge: bool, yes: bool, output: Output) -> Code {
    let mut registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory-hub: cannot load registry: {e}");
            return Code::Internal;
        }
    };

    if registry.has_consumers() && !yes {
        match output {
            Output::Json => {
                let consumers: Vec<_> = registry
                    .consumers
                    .iter()
                    .map(
                        |c| serde_json::json!({"name": c.name, "required_major": c.required_major}),
                    )
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "action": "uninstall",
                        "blocked": true,
                        "consumers": consumers,
                        "repositories": registry.repositories,
                        "message": "Other consumers are registered. Use --yes to force."
                    })
                );
            }
            Output::Human => {
                eprintln!("The following consumers are still registered:");
                for consumer in &registry.consumers {
                    eprintln!(
                        "  - {} (requires major {})",
                        consumer.name, consumer.required_major
                    );
                }
                if !registry.repositories.is_empty() {
                    eprintln!("\nKnown repositories:");
                    for repo in &registry.repositories {
                        eprintln!("  - {repo}");
                    }
                }
                eprintln!("\nUninstalling will remove the binary from the registry.");
                if purge {
                    eprintln!("--purge will ALSO delete config, models, and registry.");
                } else {
                    eprintln!("Canonical data (refs, config, models) will be PRESERVED.");
                    eprintln!("Use --purge to remove everything.");
                }
                eprintln!("Run again with --yes to confirm.");
            }
        }
        return Code::Usage;
    }

    registry.clear_installation();
    if purge {
        registry.consumers.clear();
        registry.repositories.clear();
    }

    if let Err(e) = registry.save() {
        eprintln!("memory-hub: cannot save registry: {e}");
        return Code::Internal;
    }

    match output {
        Output::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "uninstalled": true,
                    "purged": purge,
                    "dataPreserved": !purge,
                })
            );
        }
        Output::Human => {
            println!("memory-hub uninstalled from registry.");
            if purge {
                println!("All memory-hub data has been purged.");
            } else {
                println!("Canonical data (refs, config, models) preserved.");
                println!("Use --purge to remove everything.");
            }
        }
    }
    Code::Success
}

fn list(output: Output) -> Code {
    let registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory-hub: cannot load registry: {e}");
            return Code::Internal;
        }
    };
    match output {
        Output::Json => {
            let json = serde_json::to_value(&registry).unwrap_or(serde_json::Value::Null);
            println!("{json}");
        }
        Output::Human => {
            if let Some(installation) = &registry.installation {
                println!("Installation:");
                println!("  version: {}", installation.version);
                println!("  binary:  {}", installation.binary_path);
                println!("  installed: {}", installation.installed_at);
                if let Some(checksum) = &installation.checksum {
                    println!("  checksum: {checksum}");
                }
            } else {
                println!("No installation recorded.");
            }
            println!();
            if registry.consumers.is_empty() {
                println!("Consumers: (none)");
            } else {
                println!("Consumers ({}):", registry.consumer_count());
                for consumer in &registry.consumers {
                    println!(
                        "  - {} (requires major {}, registered {})",
                        consumer.name, consumer.required_major, consumer.registered_at
                    );
                }
            }
            println!();
            if registry.repositories.is_empty() {
                println!("Repositories: (none)");
            } else {
                println!("Repositories ({}):", registry.repositories.len());
                for repo in &registry.repositories {
                    println!("  - {repo}");
                }
            }
        }
    }
    Code::Success
}

fn register_consumer(name: &str, major: u16, output: Output) -> Code {
    let mut registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory-hub: cannot load registry: {e}");
            return Code::Internal;
        }
    };
    let timestamp = now_iso();
    registry.register_consumer(name, major, &timestamp);
    if let Err(e) = registry.save() {
        eprintln!("memory-hub: cannot save registry: {e}");
        return Code::Internal;
    }
    match output {
        Output::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "registered": true,
                    "consumer": name,
                    "required_major": major,
                    "registered_at": timestamp,
                })
            );
        }
        Output::Human => {
            println!("Registered consumer '{name}' (requires major {major}).");
        }
    }
    Code::Success
}

fn unregister_consumer(name: &str, output: Output) -> Code {
    let mut registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory-hub: cannot load registry: {e}");
            return Code::Internal;
        }
    };
    if !registry.unregister_consumer(name) {
        match output {
            Output::Json => {
                println!(
                    "{}",
                    serde_json::json!({"unregistered": false, "consumer": name, "reason": "not_found"})
                );
            }
            Output::Human => {
                eprintln!("Consumer '{name}' is not registered.");
            }
        }
        return Code::Usage;
    }
    if let Err(e) = registry.save() {
        eprintln!("memory-hub: cannot save registry: {e}");
        return Code::Internal;
    }
    match output {
        Output::Json => {
            println!(
                "{}",
                serde_json::json!({"unregistered": true, "consumer": name})
            );
        }
        Output::Human => {
            println!("Unregistered consumer '{name}'.");
            println!("memory-hub binary and data are NOT removed.");
        }
    }
    Code::Success
}

fn add_repository(path: &std::path::Path, output: Output) -> Code {
    let mut registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory-hub: cannot load registry: {e}");
            return Code::Internal;
        }
    };
    let path_str = path.to_string_lossy().into_owned();
    registry.register_repository(&path_str);
    if let Err(e) = registry.save() {
        eprintln!("memory-hub: cannot save registry: {e}");
        return Code::Internal;
    }
    match output {
        Output::Json => {
            println!(
                "{}",
                serde_json::json!({"added": true, "repository": path_str})
            );
        }
        Output::Human => {
            println!("Registered repository '{path_str}'.");
        }
    }
    Code::Success
}

fn remove_repository(path: &std::path::Path, output: Output) -> Code {
    let mut registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory-hub: cannot load registry: {e}");
            return Code::Internal;
        }
    };
    let path_str = path.to_string_lossy().into_owned();
    if !registry.unregister_repository(&path_str) {
        match output {
            Output::Json => {
                println!(
                    "{}",
                    serde_json::json!({"removed": false, "repository": path_str, "reason": "not_found"})
                );
            }
            Output::Human => {
                eprintln!("Repository '{path_str}' is not registered.");
            }
        }
        return Code::Usage;
    }
    if let Err(e) = registry.save() {
        eprintln!("memory-hub: cannot save registry: {e}");
        return Code::Internal;
    }
    match output {
        Output::Json => {
            println!(
                "{}",
                serde_json::json!({"removed": true, "repository": path_str})
            );
        }
        Output::Human => {
            println!("Removed repository '{path_str}' from registry.");
        }
    }
    Code::Success
}

/// Self-register the current binary as the installation. Called when memory-hub
/// starts and no installation is recorded.
#[allow(dead_code)]
pub fn self_register() {
    let mut registry = match InstallationRegistry::load() {
        Ok(r) => r,
        Err(_) => InstallationRegistry::new(),
    };
    if registry.installation.is_some() {
        return;
    }
    if let Some(binary_path) = current_binary_path() {
        let checksum = file_checksum(std::path::Path::new(&binary_path)).ok();
        registry.set_installation(InstallationRecord {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            binary_path,
            installed_at: now_iso(),
            checksum,
        });
        let _ = registry.save();
    }
}
