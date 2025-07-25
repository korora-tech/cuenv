use clap::{Parser, Subcommand};
use cuenv::constants::{CUENV_CAPABILITIES_VAR, CUENV_ENV_VAR, ENV_CUE_FILENAME};
use cuenv::errors::{Error, Result};
use cuenv::platform::{PlatformOps, Shell};
use cuenv::shell::ShellType;
use cuenv::state::StateManager;
use cuenv::{
    directory::DirectoryManager, env_manager::EnvManager, shell_hook::ShellHook,
    task_executor::TaskExecutor,
};
use std::env;
use std::path::PathBuf;

// Import the platform-specific implementation
#[cfg(unix)]
use cuenv::platform::UnixPlatform as Platform;
#[cfg(windows)]
use cuenv::platform::WindowsPlatform as Platform;

#[derive(Parser)]
#[command(name = "cuenv")]
#[command(about = "A direnv alternative using CUE files", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Load {
        #[arg(short, long)]
        directory: Option<PathBuf>,

        /// Environment to use (e.g., dev, staging, production)
        #[arg(short = 'e', long = "env")]
        environment: Option<String>,

        /// Capabilities to enable (can be specified multiple times)
        #[arg(short = 'c', long = "capability")]
        capabilities: Vec<String>,
    },
    Unload,
    Status,
    Init {
        shell: String,
    },
    Allow {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    Deny {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    Run {
        /// Environment to use (e.g., dev, staging, production)
        #[arg(short = 'e', long = "env")]
        environment: Option<String>,

        /// Capabilities to enable (can be specified multiple times)
        #[arg(short = 'c', long = "capability")]
        capabilities: Vec<String>,

        /// Task name to execute
        task_name: Option<String>,

        /// Arguments to pass to the task (after --)
        #[arg(last = true)]
        task_args: Vec<String>,

        /// Run in audit mode to see file and network access without restrictions
        #[arg(long)]
        audit: bool,
    },
    Exec {
        /// Environment to use (e.g., dev, staging, production)
        #[arg(short = 'e', long = "env")]
        environment: Option<String>,

        /// Capabilities to enable (can be specified multiple times)
        #[arg(short = 'c', long = "capability")]
        capabilities: Vec<String>,

        /// Command to run
        command: String,

        /// Arguments to pass to the command
        args: Vec<String>,

        /// Run in audit mode to see file and network access without restrictions
        #[arg(long)]
        audit: bool,
    },
    /// Generate shell hook
    Hook {
        /// Shell name (defaults to current shell)
        shell: Option<String>,
    },
    /// Export environment variables for the current directory
    Export {
        /// Shell format (defaults to current shell)
        shell: Option<String>,
    },
    /// Dump complete environment
    Dump {
        /// Shell format (defaults to current shell)
        shell: Option<String>,
    },
    /// Prune stale state
    Prune,
    /// Clear task cache
    ClearCache,
}

fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Load {
            directory,
            environment,
            capabilities,
        }) => {
            let dir = match directory {
                Some(d) => d,
                None => match env::current_dir() {
                    Ok(d) => d,
                    Err(e) => {
                        return Err(Error::file_system(
                            PathBuf::from("."),
                            "get current directory",
                            e,
                        ));
                    }
                },
            };
            let mut env_manager = EnvManager::new();

            // Use environment variables as fallback if CLI args not provided
            let env_name = environment.or_else(|| env::var(CUENV_ENV_VAR).ok());

            let mut caps = capabilities;
            if caps.is_empty() {
                // Check for CUENV_CAPABILITIES env var (comma-separated)
                if let Ok(env_caps) = env::var(CUENV_CAPABILITIES_VAR) {
                    caps = env_caps
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }

            // Load environment with options
            match env_manager.load_env_with_options(&dir, env_name, caps, None) {
                Ok(()) => {}
                Err(e) => return Err(e),
            }

            let shell = Platform::get_current_shell()
                .unwrap_or(Shell::Bash)
                .as_str();

            match env_manager.export_for_shell(shell) {
                Ok(output) => print!("{output}"),
                Err(e) => return Err(e),
            }
        }
        Some(Commands::Unload) => {
            let mut env_manager = EnvManager::new();
            match env_manager.unload_env() {
                Ok(()) => {}
                Err(e) => return Err(e),
            }

            let shell = Platform::get_current_shell()
                .unwrap_or(Shell::Bash)
                .as_str();

            match env_manager.export_for_shell(shell) {
                Ok(output) => print!("{output}"),
                Err(e) => return Err(e),
            }
        }
        Some(Commands::Status) => {
            let env_manager = EnvManager::new();
            match env_manager.print_env_diff() {
                Ok(()) => {}
                Err(e) => return Err(e),
            }
        }
        Some(Commands::Init { shell }) => match ShellHook::generate_hook(&shell) {
            Ok(output) => print!("{output}"),
            Err(e) => return Err(e),
        },
        Some(Commands::Allow { directory }) => {
            let dir_manager = DirectoryManager::new();
            let abs_dir = if directory.is_absolute() {
                directory
            } else {
                env::current_dir()?.join(directory)
            };
            match dir_manager.allow_directory(&abs_dir) {
                Ok(()) => println!("✓ Allowed directory: {}", abs_dir.display()),
                Err(e) => return Err(e),
            }
        }
        Some(Commands::Deny { directory }) => {
            let dir_manager = DirectoryManager::new();
            let abs_dir = if directory.is_absolute() {
                directory
            } else {
                env::current_dir()?.join(directory)
            };
            match dir_manager.deny_directory(&abs_dir) {
                Ok(()) => println!("✓ Denied directory: {}", abs_dir.display()),
                Err(e) => return Err(e),
            }
        }
        Some(Commands::Run {
            environment,
            capabilities,
            task_name,
            task_args,
            audit,
        }) => {
            let current_dir = match env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    return Err(Error::file_system(
                        PathBuf::from("."),
                        "get current directory",
                        e,
                    ));
                }
            };
            let mut env_manager = EnvManager::new();

            // Use environment variables as fallback if CLI args not provided
            let env_name = environment.or_else(|| env::var(CUENV_ENV_VAR).ok());

            let mut caps = capabilities;
            if caps.is_empty() {
                // Check for CUENV_CAPABILITIES env var (comma-separated)
                if let Ok(env_caps) = env::var(CUENV_CAPABILITIES_VAR) {
                    caps = env_caps
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }

            // Load environment with options
            env_manager.load_env_with_options(&current_dir, env_name, caps, None)?;

            match task_name {
                Some(name) => {
                    // Check if this is a defined task first
                    if env_manager.get_task(&name).is_some() {
                        // Execute the specified task
                        let executor = TaskExecutor::new(env_manager, current_dir);
                        let rt = tokio::runtime::Runtime::new().map_err(|e| {
                            Error::configuration(format!("Failed to create async runtime: {e}"))
                        })?;

                        let status = if audit {
                            rt.block_on(executor.execute_task_with_audit(&name, &task_args))?
                        } else {
                            rt.block_on(executor.execute_task(&name, &task_args))?
                        };
                        std::process::exit(status);
                    } else {
                        // Treat as direct command execution without restrictions
                        // For restrictions, use task definitions with security config
                        let mut args = vec![name];
                        args.extend(task_args);

                        // For direct command execution, use the first argument as command
                        if args.is_empty() {
                            return Err(Error::configuration("No command provided".to_string()));
                        }

                        let command = &args[0];
                        let command_args = &args[1..];

                        // Execute the command without restrictions for direct execution
                        let status = env_manager.run_command(command, command_args)?;
                        std::process::exit(status);
                    }
                }
                None => {
                    // List available tasks
                    let tasks = env_manager.list_tasks();
                    if tasks.is_empty() {
                        println!("No tasks defined in the CUE package");
                    } else {
                        println!("Available tasks:");
                        for (name, description) in tasks {
                            match description {
                                Some(desc) => println!("  {name}: {desc}"),
                                None => println!("  {name}"),
                            }
                        }
                    }
                }
            }
        }
        Some(Commands::Exec {
            environment,
            capabilities,
            command,
            args,
            audit,
        }) => {
            let current_dir = match env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    return Err(Error::file_system(
                        PathBuf::from("."),
                        "get current directory",
                        e,
                    ));
                }
            };
            let mut env_manager = EnvManager::new();

            // Use environment variables as fallback if CLI args not provided
            let env_name = environment.or_else(|| env::var(CUENV_ENV_VAR).ok());

            let mut caps = capabilities;
            if caps.is_empty() {
                // Check for CUENV_CAPABILITIES env var (comma-separated)
                if let Ok(env_caps) = env::var(CUENV_CAPABILITIES_VAR) {
                    caps = env_caps
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }

            // Load environment with options and command for inference
            env_manager.load_env_with_options(&current_dir, env_name, caps, Some(&command))?;

            // Execute the command with the loaded environment
            if audit {
                // For exec audit mode, create a temporary restriction object
                use cuenv::access_restrictions::AccessRestrictions;
                let _restrictions = AccessRestrictions::default();

                // Use the env_manager's run_command but with audit monitoring
                println!("🔍 Running command in audit mode...");

                // Create a simple audit by running the command and capturing output
                // For a more comprehensive audit, we'd need to integrate strace monitoring
                // into the env_manager's run_command method
                println!("⚠️  Basic audit mode - run with task definition for full system call monitoring");
                let status = env_manager.run_command(&command, &args)?;
                std::process::exit(status);
            } else {
                // Execute without restrictions for direct exec
                let status = env_manager.run_command(&command, &args)?;
                std::process::exit(status);
            }
        }
        Some(Commands::Hook { shell }) => {
            let shell_type = match shell {
                Some(s) => ShellType::from_name(&s),
                None => {
                    // Try to detect from $0
                    if let Some(arg0) = env::args().next() {
                        ShellType::detect_from_arg(&arg0)
                    } else {
                        // Fallback to platform detection
                        match Platform::get_current_shell() {
                            Ok(Shell::Bash) => ShellType::Bash,
                            Ok(Shell::Zsh) => ShellType::Zsh,
                            Ok(Shell::Fish) => ShellType::Fish,
                            Ok(Shell::PowerShell) => ShellType::PowerShell,
                            Ok(Shell::Cmd) => ShellType::Cmd,
                            _ => ShellType::Bash,
                        }
                    }
                }
            };

            let shell_impl = shell_type.as_shell();

            // Check if we should load/unload based on current directory
            let current_dir = env::current_dir()?;

            if StateManager::should_unload(&current_dir) {
                // Output unload commands
                if let Ok(Some(diff)) = StateManager::get_diff() {
                    for key in diff.removed() {
                        println!("{}", shell_impl.unset(key));
                    }
                    for (key, _) in diff.added_or_changed() {
                        if diff.prev.contains_key(key) {
                            // Restore original value
                            if let Some(orig_value) = diff.prev.get(key) {
                                println!("{}", shell_impl.export(key, orig_value));
                            }
                        } else {
                            // Variable was added, remove it
                            println!("{}", shell_impl.unset(key));
                        }
                    }
                }
                StateManager::unload()
                    .map_err(|e| Error::configuration(format!("Failed to unload state: {e}")))?;
            } else if current_dir.join(ENV_CUE_FILENAME).exists() {
                // Check if directory is allowed
                let dir_manager = DirectoryManager::new();
                if dir_manager
                    .is_directory_allowed(&current_dir)
                    .unwrap_or(false)
                {
                    // Check if files have changed and reload if needed
                    if StateManager::files_changed() || StateManager::should_load(&current_dir) {
                        // Need to load/reload
                        let mut env_manager = EnvManager::new();
                        if let Err(e) = env_manager.load_env(&current_dir) {
                            eprintln!("# cuenv: failed to load environment: {e}");
                        } else {
                            // Output export commands
                            if let Ok(Some(diff)) = StateManager::get_diff() {
                                for (key, value) in diff.added_or_changed() {
                                    println!("{}", shell_impl.export(key, value));
                                }
                                for key in diff.removed() {
                                    println!("{}", shell_impl.unset(key));
                                }
                            }
                        }
                    }
                } else {
                    eprintln!("# cuenv: Directory not allowed. Run 'cuenv allow {}' to allow this directory.", current_dir.display());
                }
            }
        }
        Some(Commands::Export { shell }) => {
            let shell_type = match shell {
                Some(s) => ShellType::from_name(&s),
                None => match Platform::get_current_shell() {
                    Ok(Shell::Bash) => ShellType::Bash,
                    Ok(Shell::Zsh) => ShellType::Zsh,
                    Ok(Shell::Fish) => ShellType::Fish,
                    Ok(Shell::PowerShell) => ShellType::PowerShell,
                    Ok(Shell::Cmd) => ShellType::Cmd,
                    _ => ShellType::Bash,
                },
            };

            let shell_impl = shell_type.as_shell();

            // Output current cuenv state as exports
            if let Ok(Some(diff)) = StateManager::get_diff() {
                for (key, value) in &diff.next {
                    if !diff.prev.contains_key(key) || diff.prev.get(key) != Some(value) {
                        println!("{}", shell_impl.export(key, value));
                    }
                }
            } else {
                eprintln!("# No cuenv environment loaded");
            }
        }
        Some(Commands::Dump { shell }) => {
            let shell_type = match shell {
                Some(s) => ShellType::from_name(&s),
                None => match Platform::get_current_shell() {
                    Ok(Shell::Bash) => ShellType::Bash,
                    Ok(Shell::Zsh) => ShellType::Zsh,
                    Ok(Shell::Fish) => ShellType::Fish,
                    Ok(Shell::PowerShell) => ShellType::PowerShell,
                    Ok(Shell::Cmd) => ShellType::Cmd,
                    _ => ShellType::Bash,
                },
            };

            let shell_impl = shell_type.as_shell();

            // Dump entire environment
            let current_env: std::collections::HashMap<String, String> = env::vars().collect();
            println!("{}", shell_impl.dump(&current_env));
        }
        Some(Commands::Prune) => {
            // For now, just unload if there's state
            if StateManager::is_loaded() {
                StateManager::unload()
                    .map_err(|e| Error::configuration(format!("Failed to unload state: {e}")))?;
                println!("Pruned cuenv state");
            } else {
                println!("No cuenv state to prune");
            }
        }
        Some(Commands::ClearCache) => {
            let task_cache = cuenv::task_cache::TaskCache::new();
            match task_cache.clear() {
                Ok(()) => println!("✓ Task cache cleared"),
                Err(e) => {
                    eprintln!("Failed to clear task cache: {e}");
                    return Err(e);
                }
            }
        }
        None => {
            let current_dir = match DirectoryManager::get_current_directory() {
                Ok(d) => d,
                Err(e) => {
                    return Err(Error::configuration(format!(
                        "failed to get current directory: {e}"
                    )));
                }
            };

            let mut env_manager = EnvManager::new();
            match env_manager.load_env(&current_dir) {
                Ok(()) => println!("cuenv: loaded CUE package from {}", current_dir.display()),
                Err(e) => {
                    eprintln!("cuenv: failed to load CUE package: {e}");
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}
