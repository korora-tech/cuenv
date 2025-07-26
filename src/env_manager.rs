use crate::errors::{Error, Result};
use crate::sync_env::SyncEnv;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::access_restrictions::AccessRestrictions;
use crate::command_executor::CommandExecutor;
use crate::cue_parser::{CommandConfig, CueParser, HookConfig, HookType, ParseOptions, TaskConfig};
use crate::env_diff::EnvDiff;
use crate::file_times::FileTimes;
use crate::hook_manager::HookManager;
use crate::output_filter::OutputFilter;
use crate::platform::{PlatformOps, Shell};
use crate::secrets::SecretManager;
use crate::state::StateManager;
use crate::types::{CommandArguments, EnvironmentVariables};
use async_trait::async_trait;
use tokio::runtime::Runtime;

// Import the platform-specific implementation
#[cfg(unix)]
use crate::platform::UnixPlatform as Platform;
#[cfg(windows)]
use crate::platform::WindowsPlatform as Platform;

pub struct EnvManager {
    original_env: HashMap<String, String>,
    cue_vars: HashMap<String, String>,
    commands: HashMap<String, CommandConfig>,
    tasks: HashMap<String, TaskConfig>,
    hooks: HashMap<String, HookConfig>,
}

impl EnvManager {
    pub fn new() -> Self {
        Self {
            original_env: HashMap::new(),
            cue_vars: HashMap::new(),
            commands: HashMap::new(),
            tasks: HashMap::new(),
            hooks: HashMap::new(),
        }
    }
}

impl Default for EnvManager {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvManager {
    pub async fn load_env(&mut self, dir: &Path) -> Result<()> {
        self.load_env_with_options(dir, None, Vec::new(), None)
            .await
    }

    pub async fn load_env_with_options(
        &mut self,
        dir: &Path,
        environment: Option<String>,
        mut capabilities: Vec<String>,
        command: Option<&str>,
    ) -> Result<()> {
        self.save_original_env()?;

        // First pass: load package to get command mappings
        let temp_options = ParseOptions {
            environment: environment.clone(),
            capabilities: Vec::new(), // Empty for now to get all commands
        };

        let parse_result = CueParser::eval_package_with_options(dir, "env", &temp_options)?;
        self.commands.extend(parse_result.commands);
        self.tasks.extend(parse_result.tasks);
        self.hooks.extend(parse_result.hooks);

        // If no capabilities were specified, try to infer from the command
        if capabilities.is_empty() {
            if let Some(cmd) = command {
                // Look up the command in our commands configuration
                if let Some(cmd_config) = self.commands.get(cmd) {
                    if let Some(cmd_caps) = &cmd_config.capabilities {
                        log::info!("Inferred capabilities for command '{cmd}': {cmd_caps:?}");
                        capabilities = cmd_caps.clone();
                    }
                }
            }

            if capabilities.is_empty() {
                log::info!(
                    "No capabilities specified or inferred, will load all non-capability-tagged variables"
                );
            }
        }

        // Second pass: load with actual capabilities
        let options = ParseOptions {
            environment,
            capabilities,
        };

        log::info!(
            "Loading CUE package from: {} with env={:?}, capabilities={:?}",
            dir.display(),
            options.environment,
            options.capabilities
        );

        match self
            .apply_cue_package_with_options(dir, "env", &options)
            .await
        {
            Ok(()) => {
                // Execute onEnter hooks after environment variables are set
                self.execute_on_enter_hooks()?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn unload_env(&mut self) -> Result<()> {
        // Execute onExit hooks before unloading environment
        let exit_hooks: Vec<_> = self
            .hooks
            .iter()
            .filter(|(_, config)| config.hook_type == HookType::OnExit)
            .collect();

        if !exit_hooks.is_empty() {
            log::info!("Executing {} onExit hooks", exit_hooks.len());
            let rt = Runtime::new()
                .map_err(|e| Error::configuration(format!("Failed to create runtime: {e}")))?;

            // Create command executor and hook manager
            let executor = Arc::new(crate::command_executor::SystemCommandExecutor::new());
            let hook_manager = match HookManager::new(executor) {
                Ok(hm) => hm,
                Err(e) => {
                    log::error!("Failed to create hook manager: {e}");
                    return Err(Error::configuration(format!(
                        "Failed to create hook manager: {e}"
                    )));
                }
            };

            // Get current environment variables for hook execution
            let current_env_vars: HashMap<String, String> = SyncEnv::vars()
                .map_err(|e| Error::Configuration {
                    message: format!("Failed to get environment variables: {}", e),
                })?
                .into_iter()
                .collect();

            for (name, config) in exit_hooks {
                log::debug!("Executing onExit hook: {name}");
                match rt.block_on(hook_manager.execute_hook(config, &current_env_vars)) {
                    Ok(_) => log::info!("Successfully executed onExit hook: {name}"),
                    Err(e) => log::error!("Failed to execute onExit hook {name}: {e}"),
                }
            }
        }

        // Restore original environment
        let current_env: Vec<(String, String)> =
            SyncEnv::vars().map_err(|e| Error::Configuration {
                message: format!("Failed to get environment variables: {}", e),
            })?;

        for (key, _) in current_env {
            if let Some(original_value) = self.original_env.get(&key) {
                SyncEnv::set_var(&key, original_value).map_err(|e| Error::Configuration {
                    message: format!("Failed to get environment variables: {}", e),
                })?;
            } else if !self.original_env.contains_key(&key) {
                SyncEnv::remove_var(&key).map_err(|e| Error::Configuration {
                    message: format!("Failed to get environment variables: {}", e),
                })?;
            }
        }

        // Clear CUE vars
        self.cue_vars.clear();

        Ok(())
    }

    fn save_original_env(&mut self) -> Result<()> {
        self.original_env = SyncEnv::vars()
            .map_err(|e| Error::Configuration {
                message: format!("Failed to get environment variables: {}", e),
            })?
            .into_iter()
            .collect();
        Ok(())
    }

    async fn apply_cue_package_with_options(
        &mut self,
        dir: &Path,
        package_name: &str,
        options: &ParseOptions,
    ) -> Result<()> {
        // Only allow loading the "env" package
        if package_name != "env" {
            return Err(Error::configuration(format!(
                "Only 'env' package is supported, got '{package_name}'. Please ensure your .cue files use 'package env'"
            )));
        }
        let parse_result = match CueParser::eval_package_with_options(dir, package_name, options) {
            Ok(result) => result,
            Err(e) => {
                return Err(Error::cue_parse_with_source(
                    dir,
                    format!("Failed to evaluate CUE package: {}", dir.display()),
                    e,
                ));
            }
        };

        // Store commands, tasks and hooks for later use
        self.commands.extend(parse_result.commands);
        self.tasks.extend(parse_result.tasks);
        self.hooks.extend(parse_result.hooks);

        // Build the new environment
        let mut new_env = self.original_env.clone();
        self.cue_vars.clear();
        for (key, value) in parse_result.variables {
            let expanded_value = match shellexpand::full(&value) {
                Ok(expanded) => expanded.to_string(),
                Err(e) => {
                    return Err(Error::shell_expansion(
                        &value,
                        format!("Failed to expand value for {key}: {e}"),
                    ));
                }
            };

            log::debug!("Setting {key}={expanded_value}");
            new_env.insert(key.clone(), expanded_value.clone());
            self.cue_vars.insert(key.clone(), expanded_value.clone());
            SyncEnv::set_var(key, expanded_value).map_err(|e| Error::Configuration {
                message: format!("Failed to get environment variables: {}", e),
            })?;
        }

        // Create environment diff
        let diff = EnvDiff::new(self.original_env.clone(), new_env);

        // Create file watches
        let mut watches = FileTimes::new();
        let env_cue = dir.join("env.cue");
        if env_cue.exists() {
            watches.watch(&env_cue);
        }

        // Save state
        StateManager::load(
            dir,
            &env_cue,
            options.environment.as_deref(),
            &options.capabilities,
            &diff,
            &watches,
        )
        .await
        .map_err(|e| Error::configuration(format!("Failed to save state: {e}")))?;

        Ok(())
    }

    pub fn print_env_diff(&self) -> Result<()> {
        let current_env: HashMap<String, String> = SyncEnv::vars()
            .map_err(|e| Error::Configuration {
                message: format!("Failed to get environment variables: {}", e),
            })?
            .into_iter()
            .collect();

        println!("Environment changes:");

        for (key, value) in &current_env {
            if let Some(original) = self.original_env.get(key) {
                if original != value {
                    println!("  {key} (modified): {original} -> {value}");
                }
            } else {
                println!("  {key} (new): {value}");
            }
        }

        for (key, value) in &self.original_env {
            if !current_env.contains_key(key) {
                println!("  {key} (removed): {value}");
            }
        }

        Ok(())
    }

    pub fn export_for_shell(&self, shell: &str) -> Result<String> {
        let current_env: HashMap<String, String> = SyncEnv::vars()
            .map_err(|e| Error::Configuration {
                message: format!("Failed to get environment variables: {}", e),
            })?
            .into_iter()
            .collect();
        let mut output = String::new();

        // Parse shell type
        let shell_type = match shell.parse::<Shell>() {
            Ok(st) => st,
            Err(_) => {
                return Err(Error::unsupported(
                    "shell",
                    format!("Unsupported shell: {shell}"),
                ));
            }
        };

        // Get export format for the shell
        let format = Platform::get_export_format(shell_type);

        // Export new or changed variables
        for (key, value) in &current_env {
            if !self.original_env.contains_key(key as &str)
                || self.original_env.get(key as &str) != Some(value)
            {
                output.push_str(&format.format_export(key, value));
                output.push('\n');
            }
        }

        // Unset removed variables
        for key in self.original_env.keys() {
            if !current_env.contains_key(key) {
                output.push_str(&format.format_unset(key));
                output.push('\n');
            }
        }

        Ok(output)
    }

    pub fn run_command(&self, command: &str, args: &[String]) -> Result<i32> {
        // Get the loaded environment variables (only the ones from CUE files)
        let env_from_cue = self.cue_vars.clone();

        // Resolve secrets in the environment variables
        let (resolved_env, secret_values) = if cfg!(test) {
            // Skip secret resolution in tests
            use crate::types::SecretValues;
            (env_from_cue, SecretValues::new())
        } else {
            let secret_manager = SecretManager::new();
            let rt = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(e) => {
                    return Err(Error::configuration(format!(
                        "Failed to create tokio runtime: {e}"
                    )));
                }
            };

            let resolved_secrets =
                match rt.block_on(secret_manager.resolve_secrets(env_from_cue.into())) {
                    Ok(secrets) => secrets,
                    Err(e) => {
                        return Err(Error::secret_resolution(
                            "multiple",
                            format!("Failed to resolve secrets: {e}"),
                        ));
                    }
                };
            (
                resolved_secrets.env_vars.into_inner(),
                resolved_secrets.secret_values,
            )
        };

        // Add minimal required environment variables for basic operation
        let mut final_env = resolved_env;

        // PATH is needed to find executables
        if let Some(path) = self.original_env.get("PATH") {
            final_env.insert("PATH".to_string(), path.clone());
        }

        // Set up platform-specific environment
        Platform::setup_environment(&mut final_env);

        // Ensure HOME directory is available (platform-specific)
        let home_var = Platform::home_env_var();
        if let Some(home_value) = self.original_env.get(home_var) {
            final_env.insert(home_var.to_string(), home_value.clone());
        }

        // Ensure HOME is set on all platforms for compatibility
        if let Some(home) = self.original_env.get("HOME") {
            final_env.insert("HOME".to_string(), home.clone());
        }

        // Create shared secret values for output filtering
        let mut secret_set = HashSet::new();
        for secret in secret_values.iter() {
            secret_set.insert(secret.to_string());
        }
        let secrets = Arc::new(Mutex::new(secret_set));

        // Create and execute the command with only the CUE environment
        let mut cmd = Command::new(command);
        cmd.args(args)
            .env_clear() // Clear all environment variables
            .envs(&final_env) // Set only our CUE-defined vars with resolved secrets
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Configure process group for better cleanup on Unix
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    format!("Failed to spawn command: {e}"),
                    None,
                ));
            }
        };

        // Set up filtered output streams
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "Failed to capture stdout".to_string(),
                    None,
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(s) => s,
            None => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "Failed to capture stderr".to_string(),
                    None,
                ));
            }
        };

        let stdout_secrets = Arc::clone(&secrets);
        let stderr_secrets = Arc::clone(&secrets);

        // Spawn threads to handle output filtering
        let stdout_thread = std::thread::spawn(move || {
            let mut filter = OutputFilter::new(io::stdout(), stdout_secrets);
            io::copy(&mut BufReader::new(stdout), &mut filter)
        });

        let stderr_thread = std::thread::spawn(move || {
            let mut filter = OutputFilter::new(io::stderr(), stderr_secrets);
            io::copy(&mut BufReader::new(stderr), &mut filter)
        });

        // Wait for the process to complete
        let status = match child.wait() {
            Ok(s) => s,
            Err(e) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    format!("Failed to wait for command: {e}"),
                    None,
                ));
            }
        };

        // Wait for output threads to complete
        match stdout_thread.join() {
            Ok(result) => match result {
                Ok(_) => {}
                Err(e) => {
                    return Err(Error::command_execution(
                        command,
                        args.to_vec(),
                        format!("Failed to process stdout: {e}"),
                        status.code(),
                    ));
                }
            },
            Err(_) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "stdout thread panicked".to_string(),
                    status.code(),
                ));
            }
        }

        match stderr_thread.join() {
            Ok(result) => match result {
                Ok(_) => {}
                Err(e) => {
                    return Err(Error::command_execution(
                        command,
                        args.to_vec(),
                        format!("Failed to process stderr: {e}"),
                        status.code(),
                    ));
                }
            },
            Err(_) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "stderr thread panicked".to_string(),
                    status.code(),
                ));
            }
        }

        Ok(status.code().unwrap_or(1))
    }

    /// Run a command with access restrictions in a hermetic environment
    pub fn run_command_with_restrictions(
        &self,
        command: &str,
        args: &[String],
        restrictions: &AccessRestrictions,
    ) -> Result<i32> {
        // Get the loaded environment variables (only the ones from CUE files)
        let env_from_cue = self.cue_vars.clone();

        // Resolve secrets in the environment variables
        let (resolved_env, secret_values) = if cfg!(test) {
            // Skip secret resolution in tests
            use crate::types::SecretValues;
            (env_from_cue, SecretValues::new())
        } else {
            let secret_manager = SecretManager::new();
            let rt = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(e) => {
                    return Err(Error::configuration(format!(
                        "Failed to create tokio runtime: {e}"
                    )));
                }
            };

            let resolved_secrets =
                match rt.block_on(secret_manager.resolve_secrets(env_from_cue.into())) {
                    Ok(secrets) => secrets,
                    Err(e) => {
                        return Err(Error::secret_resolution(
                            "multiple",
                            format!("Failed to resolve secrets: {e}"),
                        ));
                    }
                };
            (
                resolved_secrets.env_vars.into_inner(),
                resolved_secrets.secret_values,
            )
        };

        // Add minimal required environment variables for basic operation
        let mut final_env = resolved_env;

        // PATH is needed to find executables
        if let Some(path) = self.original_env.get("PATH") {
            final_env.insert("PATH".to_string(), path.clone());
        }

        // Set up platform-specific environment
        Platform::setup_environment(&mut final_env);

        // Ensure HOME directory is available (platform-specific)
        let home_var = Platform::home_env_var();
        if let Some(home_value) = self.original_env.get(home_var) {
            final_env.insert(home_var.to_string(), home_value.clone());
        }

        // Ensure HOME is set on all platforms for compatibility
        if let Some(home) = self.original_env.get("HOME") {
            final_env.insert("HOME".to_string(), home.clone());
        }

        // Create shared secret values for output filtering
        let mut secret_set = HashSet::new();
        for secret in secret_values.iter() {
            secret_set.insert(secret.to_string());
        }
        let secrets = Arc::new(Mutex::new(secret_set));

        // Create and execute the command with only the CUE environment
        let mut cmd = Command::new(command);
        cmd.args(args)
            .env_clear() // Clear all environment variables
            .envs(&final_env) // Set only our CUE-defined vars with resolved secrets
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Apply access restrictions before spawning the process
        restrictions.apply_to_command(&mut cmd)?;

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    format!("Failed to spawn command: {e}"),
                    None,
                ));
            }
        };

        // Set up filtered output streams
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "Failed to capture stdout".to_string(),
                    None,
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(s) => s,
            None => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "Failed to capture stderr".to_string(),
                    None,
                ));
            }
        };

        let stdout_secrets = Arc::clone(&secrets);
        let stderr_secrets = Arc::clone(&secrets);

        // Spawn threads to handle output filtering
        let stdout_thread = std::thread::spawn(move || {
            let mut filter = OutputFilter::new(io::stdout(), stdout_secrets);
            io::copy(&mut BufReader::new(stdout), &mut filter)
        });

        let stderr_thread = std::thread::spawn(move || {
            let mut filter = OutputFilter::new(io::stderr(), stderr_secrets);
            io::copy(&mut BufReader::new(stderr), &mut filter)
        });

        // Wait for the process to complete
        let status = match child.wait() {
            Ok(s) => s,
            Err(e) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    format!("Failed to wait for command: {e}"),
                    None,
                ));
            }
        };

        // Wait for output threads to complete
        match stdout_thread.join() {
            Ok(result) => match result {
                Ok(_) => {}
                Err(e) => {
                    return Err(Error::command_execution(
                        command,
                        args.to_vec(),
                        format!("Failed to process stdout: {e}"),
                        status.code(),
                    ));
                }
            },
            Err(_) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "stdout processing thread panicked".to_string(),
                    status.code(),
                ));
            }
        }

        match stderr_thread.join() {
            Ok(result) => match result {
                Ok(_) => {}
                Err(e) => {
                    return Err(Error::command_execution(
                        command,
                        args.to_vec(),
                        format!("Failed to process stderr: {e}"),
                        status.code(),
                    ));
                }
            },
            Err(_) => {
                return Err(Error::command_execution(
                    command,
                    args.to_vec(),
                    "stderr processing thread panicked".to_string(),
                    status.code(),
                ));
            }
        }

        Ok(status.code().unwrap_or(1))
    }

    /// Get a task by name
    pub fn get_task(&self, task_name: &str) -> Option<&TaskConfig> {
        self.tasks.get(task_name)
    }

    /// List all available tasks with their descriptions
    pub fn list_tasks(&self) -> Vec<(String, Option<String>)> {
        self.tasks
            .iter()
            .map(|(name, config)| (name.clone(), config.description.clone()))
            .collect()
    }

    /// Get all tasks as a HashMap
    pub fn get_tasks(&self) -> &HashMap<String, TaskConfig> {
        &self.tasks
    }

    fn execute_on_enter_hooks(&self) -> Result<()> {
        // Filter for onEnter hooks
        let on_enter_hooks: Vec<(&String, &HookConfig)> = self
            .hooks
            .iter()
            .filter(|(_, config)| config.hook_type == HookType::OnEnter)
            .collect();

        if on_enter_hooks.is_empty() {
            return Ok(());
        }

        log::info!("Executing {} onEnter hooks", on_enter_hooks.len());

        // Collect current environment variables for hook execution
        let env_vars = self.collect_cue_env_vars()?;

        // Create runtime for async hook execution
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(e) => {
                return Err(Error::configuration(format!(
                    "Failed to create tokio runtime for hooks: {e}"
                )));
            }
        };

        // Create a command executor that uses the system's Command
        struct SystemCommandExecutor;

        #[async_trait]
        impl CommandExecutor for SystemCommandExecutor {
            async fn execute(
                &self,
                cmd: &str,
                args: &CommandArguments,
            ) -> Result<std::process::Output> {
                match std::process::Command::new(cmd)
                    .args(args.as_slice())
                    .output()
                {
                    Ok(output) => Ok(output),
                    Err(e) => Err(Error::command_execution(
                        cmd,
                        args.clone().into_inner(),
                        format!("failed to execute command: {e}"),
                        None,
                    )),
                }
            }

            async fn execute_with_env(
                &self,
                cmd: &str,
                args: &CommandArguments,
                env: EnvironmentVariables,
            ) -> Result<std::process::Output> {
                match std::process::Command::new(cmd)
                    .args(args.as_slice())
                    .envs(env.into_inner())
                    .output()
                {
                    Ok(output) => Ok(output),
                    Err(e) => Err(Error::command_execution(
                        cmd,
                        args.clone().into_inner(),
                        format!("failed to execute command with environment: {e}"),
                        None,
                    )),
                }
            }
        }

        // Execute hooks
        let executor = Arc::new(SystemCommandExecutor);
        let hook_manager = match HookManager::new(executor) {
            Ok(hm) => hm,
            Err(e) => {
                return Err(Error::configuration(format!(
                    "Failed to create hook manager: {e}"
                )));
            }
        };

        for (name, config) in on_enter_hooks {
            log::debug!("Executing onEnter hook: {name}");
            match rt.block_on(hook_manager.execute_hook(config, &env_vars)) {
                Ok(_) => log::info!("Successfully executed onEnter hook: {name}"),
                Err(e) => {
                    // Log error but continue with other hooks
                    log::error!("Failed to execute onEnter hook '{name}': {e}");
                }
            }
        }

        Ok(())
    }

    fn collect_cue_env_vars(&self) -> Result<HashMap<String, String>> {
        let current_env: HashMap<String, String> = SyncEnv::vars()
            .map_err(|e| Error::Configuration {
                message: format!("Failed to get environment variables: {}", e),
            })?
            .into_iter()
            .collect();
        let mut cue_env_vars = HashMap::new();

        // Collect variables that were added or modified by CUE
        for (key, value) in &current_env {
            if !self.original_env.contains_key(key as &str)
                || self.original_env.get(key as &str) != Some(value)
            {
                cue_env_vars.insert(key.clone(), value.clone());
            }
        }

        // Add minimal required environment variables for hook execution
        if let Some(path) = self.original_env.get("PATH") {
            cue_env_vars.insert("PATH".to_string(), path.clone());
        }

        // Ensure HOME directory is available
        let home_var = Platform::home_env_var();
        if let Some(home_value) = self.original_env.get(home_var) {
            cue_env_vars.insert(home_var.to_string(), home_value.clone());
        }

        // Ensure HOME is set on all platforms for compatibility
        if let Some(home) = self.original_env.get("HOME") {
            cue_env_vars.insert("HOME".to_string(), home.clone());
        }

        Ok(cue_env_vars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_load_and_unload_env() {
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env.cue");
        fs::write(
            &env_file,
            r#"package env

env: {
    CUENV_TEST_VAR_UNIQUE: "test_value"
}"#,
        )
        .unwrap();

        let original_value = SyncEnv::var("CUENV_TEST_VAR_UNIQUE").unwrap_or_default();

        let mut manager = EnvManager::new();
        manager.load_env(temp_dir.path()).unwrap();

        assert_eq!(
            SyncEnv::var("CUENV_TEST_VAR_UNIQUE").unwrap(),
            Some("test_value".to_string())
        );

        manager.unload_env().unwrap();

        match original_value {
            Some(val) => assert_eq!(SyncEnv::var("CUENV_TEST_VAR_UNIQUE").unwrap(), Some(val)),
            None => assert!(SyncEnv::var("CUENV_TEST_VAR_UNIQUE").unwrap().is_none()),
        }
    }

    #[test]
    fn test_run_command_hermetic() {
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env.cue");
        fs::write(
            &env_file,
            r#"package env

env: {
    TEST_FROM_CUE: "cue_value"
    PORT: "8080"
}"#,
        )
        .unwrap();

        let mut manager = EnvManager::new();
        manager.load_env(temp_dir.path()).unwrap();

        // Set a variable AFTER loading env, so it's not in original_env
        SyncEnv::set_var("TEST_PARENT_VAR", "should_not_exist").unwrap();

        // Run a command that checks for our variables
        #[cfg(unix)]
        let (cmd, args) = (
            "sh",
            vec![
                "-c".to_string(),
                "test \"$TEST_FROM_CUE\" = \"cue_value\" && test -z \"$TEST_PARENT_VAR\""
                    .to_string(),
            ],
        );

        #[cfg(windows)]
        let (cmd, args) = ("cmd", vec![
            "/C".to_string(),
            "if \"%TEST_FROM_CUE%\"==\"cue_value\" (if \"%TEST_PARENT_VAR%\"==\"\" exit 0 else exit 1) else exit 1".to_string()
        ]);

        let status = manager.run_command(cmd, &args).unwrap();

        assert_eq!(status, 0, "Command should succeed with correct environment");

        // Clean up
        let _ = SyncEnv::remove_var("TEST_PARENT_VAR");
    }

    #[test]
    fn test_run_command_with_secret_refs() {
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env.cue");

        // Write a CUE file with normal values only
        // We can't test actual secret resolution without mocking the secret managers
        fs::write(
            &env_file,
            r#"package env

env: {
    NORMAL_VAR: "normal-value"
    ANOTHER_VAR: "another-value"
}"#,
        )
        .unwrap();

        let mut manager = EnvManager::new();
        manager.load_env(temp_dir.path()).unwrap();

        // Run a command that checks the variables
        #[cfg(unix)]
        let (cmd, args) = ("sh", vec![
            "-c".to_string(),
            "test \"$NORMAL_VAR\" = \"normal-value\" && test \"$ANOTHER_VAR\" = \"another-value\"".to_string()
        ]);

        #[cfg(windows)]
        let (cmd, args) = ("cmd", vec![
            "/C".to_string(),
            "if \"%NORMAL_VAR%\"==\"normal-value\" (if \"%ANOTHER_VAR%\"==\"another-value\" exit 0 else exit 1) else exit 1".to_string()
        ]);

        let status = manager.run_command(cmd, &args).unwrap();

        assert_eq!(status, 0, "Command should succeed with all variables set");
    }

    #[test]
    fn test_run_command_preserves_path_and_home() {
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env.cue");
        fs::write(
            &env_file,
            r#"package env

env: {
    TEST_VAR: "test"
}"#,
        )
        .unwrap();

        let mut manager = EnvManager::new();
        manager.load_env(temp_dir.path()).unwrap();

        // Run a command that checks PATH and HOME are preserved
        #[cfg(unix)]
        let (cmd, args) = (
            "sh",
            vec![
                "-c".to_string(),
                "test -n \"$PATH\" && test -n \"$HOME\"".to_string(),
            ],
        );

        #[cfg(windows)]
        let (cmd, args) = (
            "cmd",
            vec![
                "/C".to_string(),
                "if defined PATH (if defined HOME exit 0 else exit 1) else exit 1".to_string(),
            ],
        );

        let status = manager.run_command(cmd, &args).unwrap();

        assert_eq!(status, 0, "PATH and HOME should be preserved");
    }

    #[test]
    fn test_run_command_with_restrictions() {
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env.cue");
        fs::write(
            &env_file,
            r#"package env

env: {
    TEST_VAR: "test"
}"#,
        )
        .unwrap();

        let mut manager = EnvManager::new();
        manager.load_env(temp_dir.path()).unwrap();

        // Test without restrictions (should work)
        let restrictions = AccessRestrictions::default();
        let status =
            manager.run_command_with_restrictions("echo", &["test".to_string()], &restrictions);

        // This should work since no restrictions are applied
        assert!(
            status.is_ok(),
            "Command should succeed without restrictions"
        );

        // Test with restrictions (may fail in test environment, but should not panic)
        let restrictions = AccessRestrictions::new(true, true);
        let result =
            manager.run_command_with_restrictions("echo", &["test".to_string()], &restrictions);

        // The result may be Ok or Err depending on environment capabilities
        // What matters is that it doesn't panic and properly handles restrictions
        match result {
            Ok(_) => {
                // Command succeeded (unlikely in restricted environment)
            }
            Err(e) => {
                // Command failed due to restrictions (expected in most test environments)
                let error_msg = e.to_string();
                // Verify the error is related to restrictions/command execution
                assert!(
                    error_msg.contains("CommandExecution")
                        || error_msg.contains("Failed to capture stdout")
                        || error_msg.contains("Failed to spawn command")
                        || error_msg.contains("Network restrictions with Landlock")
                        || error_msg.contains("configuration error"),
                    "Error should be related to command execution with restrictions: {error_msg}"
                );
            }
        }
    }

    #[test]
    fn test_access_restrictions_creation_and_methods() {
        use crate::access_restrictions::AccessRestrictions;

        // Test default (no restrictions)
        let restrictions = AccessRestrictions::default();
        assert!(!restrictions.has_any_restrictions());

        // Test with all restrictions
        let restrictions = AccessRestrictions::new(true, true);
        assert!(restrictions.has_any_restrictions());

        // Test with partial restrictions
        let restrictions = AccessRestrictions::new(true, false);
        assert!(restrictions.has_any_restrictions());

        let restrictions = AccessRestrictions::new(false, true);
        assert!(restrictions.has_any_restrictions());

        let restrictions = AccessRestrictions::new(false, false);
        assert!(!restrictions.has_any_restrictions());
    }
}
