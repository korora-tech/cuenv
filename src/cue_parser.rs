use crate::constants::ENV_PACKAGE_NAME;
use crate::errors::{Error, Result};
use crate::resilience::suggest_recovery;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

/// RAII wrapper for C strings returned from FFI
/// Ensures proper cleanup when the wrapper goes out of scope
struct CStringPtr {
    ptr: *mut c_char,
}

impl CStringPtr {
    /// Creates a new wrapper from a raw pointer
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - `ptr` is either null or a valid pointer returned from the FFI
    /// - The pointer has not been freed already
    /// - The pointer will not be used after this wrapper is dropped
    unsafe fn new(ptr: *mut c_char) -> Self {
        Self { ptr }
    }

    /// Checks if the wrapped pointer is null
    fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    /// Converts the C string to a Rust &str
    ///
    /// # Safety
    /// The caller must ensure that the wrapped pointer is not null
    unsafe fn to_str(&self) -> Result<&str> {
        debug_assert!(
            !self.is_null(),
            "Attempted to convert null pointer to string"
        );

        let cstr = CStr::from_ptr(self.ptr);
        cstr.to_str().map_err(|e| {
            Error::ffi(
                "cue_eval_package",
                format!("failed to convert C string to UTF-8: {}", e),
            )
        })
    }
}

impl Drop for CStringPtr {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // Safety: We only call cue_free_string on non-null pointers that were
            // returned from cue_eval_package. The FFI contract guarantees that
            // this is safe to call exactly once per returned pointer.
            unsafe {
                cue_free_string(self.ptr);
            }
        }
    }
}

#[link(name = "cue_bridge")]
extern "C" {
    fn cue_eval_package(dir_path: *const c_char, package_name: *const c_char) -> *mut c_char;
    fn cue_free_string(s: *mut c_char);
}

#[derive(Debug, Deserialize)]
struct CueParseResult {
    variables: HashMap<String, serde_json::Value>,
    metadata: HashMap<String, VariableMetadata>,
    environments: HashMap<String, HashMap<String, serde_json::Value>>,
    commands: HashMap<String, CommandConfig>,
    #[serde(default)]
    tasks: HashMap<String, TaskConfig>,
    hooks: Option<HooksConfig>,
}

#[derive(Debug, Deserialize)]
struct HooksConfig {
    #[serde(rename = "onEnter")]
    on_enter: Option<HookConfig>,
    #[serde(rename = "onExit")]
    on_exit: Option<HookConfig>,
}

#[derive(Debug, Deserialize)]
struct VariableMetadata {
    capability: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandConfig {
    pub capabilities: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub description: Option<String>,
    pub command: Option<String>,
    pub script: Option<String>,
    pub dependencies: Option<Vec<String>>,
    #[serde(rename = "workingDir")]
    pub working_dir: Option<String>,
    pub shell: Option<String>,
    pub inputs: Option<Vec<String>>,
    pub outputs: Option<Vec<String>>,
    pub security: Option<SecurityConfig>,
    /// Enable build cache for this task (Bazel-style caching)
    pub cache: Option<bool>,
    /// Custom cache key - if not provided, will be derived from inputs
    #[serde(rename = "cacheKey")]
    pub cache_key: Option<String>,
    /// Timeout for task execution in seconds
    pub timeout: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(rename = "restrictDisk")]
    pub restrict_disk: Option<bool>,
    #[serde(rename = "restrictNetwork")]
    pub restrict_network: Option<bool>,
    #[serde(rename = "readOnlyPaths")]
    pub read_only_paths: Option<Vec<String>>,
    #[serde(rename = "readWritePaths")]
    pub read_write_paths: Option<Vec<String>>,
    #[serde(rename = "denyPaths")]
    pub deny_paths: Option<Vec<String>>,
    #[serde(rename = "allowedHosts")]
    pub allowed_hosts: Option<Vec<String>>,
    /// Automatically infer disk restrictions from task inputs/outputs
    #[serde(rename = "inferFromInputsOutputs")]
    pub infer_from_inputs_outputs: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum HookType {
    #[default]
    OnEnter,
    OnExit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookConstraint {
    /// Check if a command is available in PATH
    CommandExists { command: String },
    /// Run a custom shell command and check if it succeeds (exit code 0)
    ShellCommand {
        command: String,
        args: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub command: String,
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub constraints: Vec<HookConstraint>,
    #[serde(skip)]
    pub hook_type: HookType,
}

pub struct CueParser;

impl CueParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CueParser {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct ParseOptions {
    pub environment: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParseResult {
    pub variables: HashMap<String, String>,
    pub commands: HashMap<String, CommandConfig>,
    pub tasks: HashMap<String, TaskConfig>,
    pub hooks: HashMap<String, HookConfig>,
}

// Input validation functions
fn validate_package_name(package_name: &str) -> Result<()> {
    if package_name.is_empty() {
        return Err(Error::configuration(
            "Package name cannot be empty".to_string(),
        ));
    }

    // Only allow loading the "env" package
    if package_name != ENV_PACKAGE_NAME {
        return Err(Error::configuration(format!(
            "Only 'env' package is supported, got '{package_name}'. Please ensure your .cue files use 'package env'"
        )));
    }

    Ok(())
}

fn validate_directory_path(dir: &Path) -> Result<String> {
    let dir_str = dir.to_string_lossy();
    if dir_str.is_empty() {
        return Err(Error::configuration(
            "Directory path cannot be empty".to_string(),
        ));
    }
    Ok(dir_str.to_string())
}

// FFI string management utilities
fn create_ffi_string(value: &str, context: &str) -> Result<CString> {
    CString::new(value).map_err(|e| {
        Error::ffi(
            "cue_eval_package",
            format!("{} - contains null byte: {}", context, e),
        )
    })
}

fn call_cue_eval_package(dir_path: &CStr, package_name: &CStr) -> *mut c_char {
    // Safety: cue_eval_package is an external C function that:
    // - Takes two non-null C string pointers as arguments
    // - Returns a heap-allocated C string that must be freed with cue_free_string
    // - Returns null on allocation failure
    // We ensure the input pointers are valid for the duration of the call
    unsafe { cue_eval_package(dir_path.as_ptr(), package_name.as_ptr()) }
}

// JSON parsing utilities
fn parse_json_response(json_str: &str) -> Result<serde_json::Value> {
    serde_json::from_str(json_str).map_err(|e| {
        let error = Error::Json {
            message: "failed to parse JSON result from CUE parser".to_string(),
            source: e,
        };
        log::error!("CUE parser returned invalid JSON: {}", json_str);
        log::error!("Recovery suggestion: {}", suggest_recovery(&error));
        error
    })
}

fn check_for_error_response(json_value: &serde_json::Value, dir: &Path) -> Result<()> {
    if let serde_json::Value::Object(ref map) = json_value {
        if let Some(serde_json::Value::String(error)) = map.get("error") {
            let cue_error = Error::cue_parse(dir, error.clone());

            // Provide specific recovery suggestions based on error content
            let recovery_hint = get_recovery_hint(error);

            log::error!("CUE parsing error: {}", error);
            log::error!("Recovery suggestion: {}", recovery_hint);
            return Err(cue_error);
        }
    }
    Ok(())
}

fn get_recovery_hint(error: &str) -> &'static str {
    if error.contains("cannot find package") {
        "Ensure your .cue files have 'package env' at the top"
    } else if error.contains("expected") || error.contains("syntax") {
        "Check for missing commas, brackets, or quotes in your CUE file"
    } else if error.contains("cycle") {
        "You have a circular dependency in your CUE definitions"
    } else if error.contains("incomplete") {
        "Some required fields are missing in your CUE configuration"
    } else {
        "Run 'cue vet' on your files to check for syntax errors"
    }
}

fn deserialize_cue_result(json_value: serde_json::Value) -> Result<CueParseResult> {
    serde_json::from_value(json_value).map_err(|e| {
        let error = Error::Json {
            message: "failed to parse CUE result structure".to_string(),
            source: e,
        };
        log::error!("Failed to deserialize CUE result. This might indicate a version mismatch.");
        log::error!("Recovery suggestion: {}", suggest_recovery(&error));
        error
    })
}

impl CueParser {
    pub fn eval_package(dir: &Path, package_name: &str) -> Result<HashMap<String, String>> {
        match Self::eval_package_with_options(dir, package_name, &ParseOptions::default()) {
            Ok(result) => Ok(result.variables),
            Err(e) => Err(e),
        }
    }

    pub fn eval_package_with_options(
        dir: &Path,
        package_name: &str,
        options: &ParseOptions,
    ) -> Result<ParseResult> {
        // Validate inputs
        validate_package_name(package_name)?;
        let dir_str = validate_directory_path(dir)?;

        // Create FFI strings
        let c_dir = create_ffi_string(&dir_str, "invalid directory path")?;
        let c_package = create_ffi_string(package_name, "invalid package name")?;

        // Call CUE evaluation
        let result_ptr = call_cue_eval_package(&c_dir, &c_package);

        // Wrap the result pointer for automatic cleanup
        // Safety: result_ptr is either null or a valid pointer returned from cue_eval_package
        let result_wrapper = unsafe { CStringPtr::new(result_ptr) };

        if result_wrapper.is_null() {
            return Err(Error::cue_parse(dir, "CUE parser returned null pointer"));
        }

        // Safety: We've verified the pointer is not null
        let result_str = unsafe { result_wrapper.to_str()? };

        let parse_result = if result_str.is_empty() {
            ParseResult::default()
        } else {
            // Parse and validate JSON response
            let json_value = parse_json_response(result_str)?;
            check_for_error_response(&json_value, dir)?;

            // Deserialize and build final result
            let cue_result = deserialize_cue_result(json_value)?;
            Self::build_parse_result(cue_result, options)?
        };

        // The CStringPtr will be automatically freed when it goes out of scope
        Ok(parse_result)
    }

    fn build_parse_result(
        mut cue_result: CueParseResult,
        options: &ParseOptions,
    ) -> Result<ParseResult> {
        let final_vars = build_filtered_variables(&cue_result, options);
        let hooks = extract_hooks(cue_result.hooks);

        Ok(ParseResult {
            variables: final_vars,
            commands: std::mem::take(&mut cue_result.commands),
            tasks: std::mem::take(&mut cue_result.tasks),
            hooks,
        })
    }

    fn value_to_string(val: &serde_json::Value) -> Option<String> {
        match val {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            serde_json::Value::Null => None,
            _ => {
                log::warn!("Skipping non-primitive value");
                None
            }
        }
    }
}

// Capability filtering logic
fn should_include_variable(
    key: &str,
    metadata: &HashMap<String, VariableMetadata>,
    capabilities: &[String],
) -> bool {
    if let Some(var_metadata) = metadata.get(key) {
        if let Some(cap) = &var_metadata.capability {
            // Variable has a capability tag, only include if it matches the filter
            capabilities.is_empty() || capabilities.contains(cap)
        } else {
            // No capability tag means always include
            true
        }
    } else {
        // No metadata means no capability tag, always include
        true
    }
}

// Variable processing functions
fn process_variables(
    variables: &HashMap<String, serde_json::Value>,
    metadata: &HashMap<String, VariableMetadata>,
    capabilities: &[String],
) -> HashMap<String, String> {
    let mut result = HashMap::with_capacity(variables.len());

    for (key, val) in variables {
        if should_include_variable(key, metadata, capabilities) {
            if let Some(str_val) = CueParser::value_to_string(val) {
                result.insert(key.clone(), str_val);
            }
        }
    }

    result
}

fn build_filtered_variables(
    cue_result: &CueParseResult,
    options: &ParseOptions,
) -> HashMap<String, String> {
    // Start with base variables
    let mut final_vars = process_variables(
        &cue_result.variables,
        &cue_result.metadata,
        &options.capabilities,
    );

    // Apply environment-specific overrides
    if let Some(env_name) = &options.environment {
        if let Some(env_vars) = cue_result.environments.get(env_name) {
            let env_overrides =
                process_variables(env_vars, &cue_result.metadata, &options.capabilities);

            // Merge environment overrides into base variables
            final_vars.extend(env_overrides);
        }
    }

    final_vars
}

// Hook processing functions
fn extract_hooks(hooks_config: Option<HooksConfig>) -> HashMap<String, HookConfig> {
    let mut hooks = HashMap::with_capacity(2); // At most 2 hooks (onEnter, onExit)

    if let Some(config) = hooks_config {
        if let Some(mut on_enter) = config.on_enter {
            on_enter.hook_type = HookType::OnEnter;
            hooks.insert("onEnter".to_string(), on_enter);
        }
        if let Some(mut on_exit) = config.on_exit {
            on_exit.hook_type = HookType::OnExit;
            hooks.insert("onExit".to_string(), on_exit);
        }
    }

    hooks
}

#[cfg(test)]
mod tests {
    // Tests for pure functions
    #[test]
    fn test_validate_package_name() {
        use super::validate_package_name;

        // Empty package name should fail
        assert!(validate_package_name("").is_err());

        // Non-env package should fail
        assert!(validate_package_name("mypackage").is_err());

        // Only "env" package should succeed
        assert!(validate_package_name("env").is_ok());
    }

    #[test]
    fn test_validate_directory_path() {
        use super::validate_directory_path;
        use std::path::Path;

        // Empty path should fail
        assert!(validate_directory_path(Path::new("")).is_err());

        // Valid path should succeed
        let result = validate_directory_path(Path::new("/tmp/test"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/tmp/test");
    }

    #[test]
    fn test_should_include_variable() {
        use super::{should_include_variable, VariableMetadata};
        use std::collections::HashMap;

        let mut metadata = HashMap::new();
        metadata.insert(
            "AWS_KEY".to_string(),
            VariableMetadata {
                capability: Some("aws".to_string()),
            },
        );
        metadata.insert("DB_URL".to_string(), VariableMetadata { capability: None });

        // Variable with no metadata should always be included
        assert!(should_include_variable("UNKNOWN", &metadata, &[]));
        assert!(should_include_variable(
            "UNKNOWN",
            &metadata,
            &["aws".to_string()]
        ));

        // Variable with no capability should always be included
        assert!(should_include_variable("DB_URL", &metadata, &[]));
        assert!(should_include_variable(
            "DB_URL",
            &metadata,
            &["aws".to_string()]
        ));

        // Variable with capability should respect filter
        assert!(should_include_variable("AWS_KEY", &metadata, &[])); // Empty filter includes all
        assert!(should_include_variable(
            "AWS_KEY",
            &metadata,
            &["aws".to_string()]
        )); // Matching capability
        assert!(!should_include_variable(
            "AWS_KEY",
            &metadata,
            &["gcp".to_string()]
        )); // Non-matching capability
    }

    #[test]
    fn test_get_recovery_hint() {
        use super::get_recovery_hint;

        assert_eq!(
            get_recovery_hint("cannot find package"),
            "Ensure your .cue files have 'package env' at the top"
        );
        assert_eq!(
            get_recovery_hint("expected token"),
            "Check for missing commas, brackets, or quotes in your CUE file"
        );
        assert_eq!(
            get_recovery_hint("cycle detected"),
            "You have a circular dependency in your CUE definitions"
        );
        assert_eq!(
            get_recovery_hint("incomplete value"),
            "Some required fields are missing in your CUE configuration"
        );
        assert_eq!(
            get_recovery_hint("unknown error"),
            "Run 'cue vet' on your files to check for syntax errors"
        );
    }
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_env(content: &str) -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let cue_dir = temp_dir.path().join("cue.mod");
        fs::create_dir(&cue_dir).unwrap();
        fs::write(cue_dir.join("module.cue"), "module: \"test.com/env\"").unwrap();

        let env_file = temp_dir.path().join("env.cue");
        fs::write(&env_file, content).unwrap();

        temp_dir
    }

    #[test]
    fn test_only_env_package_allowed() {
        // Test that non-env packages are rejected
        let content = r#"
        package mypackage
        
        env: {
            DATABASE_URL: "postgresql://localhost/mydb"
        }"#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "mypackage");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Only 'env' package is supported"),
            "Error message was: {err_msg}"
        );

        // Test that env package is accepted
        let content = r#"
        package env
        
        env: {
            DATABASE_URL: "postgresql://localhost/mydb"
        }"#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_simple_env() {
        let content = r#"
        package env

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
            API_KEY:      "secret123"
            DEBUG:        true
            PORT:         3000
        }
        "#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();

        assert_eq!(
            result.get("DATABASE_URL").unwrap(),
            "postgres://localhost/mydb"
        );
        assert_eq!(result.get("API_KEY").unwrap(), "secret123");
        assert_eq!(result.get("DEBUG").unwrap(), "true");
        assert_eq!(result.get("PORT").unwrap(), "3000");
    }

    #[test]
    fn test_parse_with_comments() {
        let content = r#"
        package env

        env: {
            // This is a comment
            DATABASE_URL: "postgres://localhost/mydb"
            // Multi-line comments in CUE use //
            // not /* */
            API_KEY: "secret123"
            // Another comment
            DEBUG: true
        }
        "#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        assert_eq!(
            result.get("DATABASE_URL").unwrap(),
            "postgres://localhost/mydb"
        );
        assert_eq!(result.get("API_KEY").unwrap(), "secret123");
        assert_eq!(result.get("DEBUG").unwrap(), "true");
    }

    #[test]
    fn test_parse_cue_features() {
        let content = r#"
        package env

        env: {
            // CUE supports string interpolation
            BASE_URL: "https://api.example.com"
            API_ENDPOINT: "\(BASE_URL)/v1"

            // Default values
            PORT: *3000 | int

            // Constraints
            TIMEOUT: >=0 & <=3600 & int | *30
        }
        "#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        // The CUE parser will evaluate these expressions
        assert!(result.contains_key("BASE_URL"));
        assert!(result.contains_key("PORT"));
    }

    #[test]
    fn test_package_requirement() {
        let content = r#"{
            env: {
                DATABASE_URL: "postgres://localhost/mydb"
            }
        }"#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_environments() {
        let content = r#"
        package env

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
            API_KEY:      "secret123"
            PORT:         3000
            
            environment: {
                production: {
                    DATABASE_URL: "postgres://prod.example.com/mydb"
                    PORT:         8080
                }
                staging: {
                    DATABASE_URL: "postgres://staging.example.com/mydb"
                    API_KEY:      "staging-key"
                }
            }
        }
        "#;
        let temp_dir = create_test_env(content);

        // Test default parsing (no environment)
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        assert_eq!(
            result.get("DATABASE_URL").unwrap(),
            "postgres://localhost/mydb"
        );
        assert_eq!(result.get("API_KEY").unwrap(), "secret123");
        assert_eq!(result.get("PORT").unwrap(), "3000");

        // Test with production environment
        let options = ParseOptions {
            environment: Some("production".to_string()),
            capabilities: Vec::new(),
        };
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(
            result.variables.get("DATABASE_URL").unwrap(),
            "postgres://prod.example.com/mydb"
        );
        assert_eq!(result.variables.get("API_KEY").unwrap(), "secret123"); // Not overridden
        assert_eq!(result.variables.get("PORT").unwrap(), "8080");

        // Test with staging environment
        let options = ParseOptions {
            environment: Some("staging".to_string()),
            capabilities: Vec::new(),
        };
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(
            result.variables.get("DATABASE_URL").unwrap(),
            "postgres://staging.example.com/mydb"
        );
        assert_eq!(result.variables.get("API_KEY").unwrap(), "staging-key");
        assert_eq!(result.variables.get("PORT").unwrap(), "3000"); // Not overridden
    }

    #[test]
    fn test_parse_with_capabilities() {
        let content = r#"
        package env

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
            API_KEY:      "secret123"
        }

        metadata: {
            AWS_ACCESS_KEY: capability: "aws"
            AWS_SECRET_KEY: capability: "aws"
        }
        "#;
        let temp_dir = create_test_env(content);

        // Test without capability filter
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains_key("DATABASE_URL"));
        assert!(result.contains_key("API_KEY"));

        // Test with aws capability filter
        let options = ParseOptions {
            environment: None,
            capabilities: vec!["aws".to_string()],
        };
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(result.variables.len(), 2); // DATABASE_URL and API_KEY have no capabilities, so they're always included

        // Test with non-existent capability
        let options = ParseOptions {
            environment: None,
            capabilities: vec!["gcp".to_string()],
        };
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(result.variables.len(), 2); // DATABASE_URL and API_KEY have no capabilities, so they're always included
    }

    #[test]
    fn test_parse_with_commands() {
        let content = r#"
        package env

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
        }

        capabilities: {
            database: {
                commands: ["migrate"]
            }
            aws: {
                commands: ["deploy"]
            }
            docker: {
                commands: ["deploy", "test"]
            }
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert!(result.commands.contains_key("migrate"));
        assert!(result.commands.contains_key("deploy"));
        assert!(result.commands.contains_key("test"));

        let migrate_cmd = &result.commands["migrate"];
        assert_eq!(
            migrate_cmd.capabilities.as_ref().unwrap(),
            &vec!["database".to_string()]
        );

        let deploy_cmd = &result.commands["deploy"];
        let mut expected_caps = vec!["aws".to_string(), "docker".to_string()];
        let mut actual_caps = deploy_cmd.capabilities.as_ref().unwrap().clone();
        expected_caps.sort();
        actual_caps.sort();
        assert_eq!(actual_caps, expected_caps);

        let test_cmd = &result.commands["test"];
        assert_eq!(
            test_cmd.capabilities.as_ref().unwrap(),
            &vec!["docker".to_string()]
        );
    }

    #[test]
    fn test_parse_with_env_and_capabilities() {
        let content = r#"
        package env

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
            API_KEY:      "secret123"
            AWS_ACCESS_KEY: "aws-key-dev"
            
            environment: {
                production: {
                    DATABASE_URL: "postgres://prod.example.com/mydb"
                    AWS_ACCESS_KEY: "aws-key-prod"
                }
            }
        }

        metadata: {
            AWS_ACCESS_KEY: capability: "aws"
        }
        "#;
        let temp_dir = create_test_env(content);

        // Test production environment with aws capability
        let options = ParseOptions {
            environment: Some("production".to_string()),
            capabilities: vec!["aws".to_string()],
        };
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(result.variables.len(), 3);
        assert_eq!(
            result.variables.get("AWS_ACCESS_KEY").unwrap(),
            "aws-key-prod"
        );
        assert_eq!(
            result.variables.get("DATABASE_URL").unwrap(),
            "postgres://prod.example.com/mydb"
        );
        assert_eq!(result.variables.get("API_KEY").unwrap(), "secret123")
    }

    #[test]
    fn test_empty_cue_file() {
        let content = r#"
        package env

        env: {}
        "#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_structured_secrets() {
        // Test with simpler CUE syntax that the parser can handle
        let content = r#"
        package env

        env: {
            // Regular variables
            DATABASE_URL: "postgres://localhost/mydb"

            // Secret references in string format
            AWS_KEY: "op://Personal/aws/key"
            DB_PASS: "op://Work/database/password"

            // Traditional secret format
            STRIPE_KEY: "op://Work/stripe/key"
            GCP_SECRET: "gcp-secret://my-project/api-key"
        }
        "#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();

        // Regular variable
        assert_eq!(
            result.get("DATABASE_URL").unwrap(),
            "postgres://localhost/mydb"
        );

        // Secret references
        assert_eq!(result.get("AWS_KEY").unwrap(), "op://Personal/aws/key");
        assert_eq!(
            result.get("DB_PASS").unwrap(),
            "op://Work/database/password"
        );

        // Traditional secret references
        assert_eq!(result.get("STRIPE_KEY").unwrap(), "op://Work/stripe/key");
        assert_eq!(
            result.get("GCP_SECRET").unwrap(),
            "gcp-secret://my-project/api-key"
        );
    }

    #[test]
    fn test_parse_with_nested_objects() {
        let content = r#"
        package env

        env: {
            DATABASE: {
                host: "localhost"
                port: 5432
            }
        }
        "#;
        let temp_dir = create_test_env(content);
        // The parser should skip non-primitive values
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_value_types() {
        let content = r#"
        package env

        env: {
            STRING_VAL: "hello"
            INT_VAL:    42
            FLOAT_VAL:  3.14
            BOOL_VAL:   true
            NULL_VAL:   null
            ARRAY_VAL: [1, 2, 3]
            OBJECT_VAL: {nested: "value"}
        }
        "#;
        let temp_dir = create_test_env(content);
        let result = CueParser::eval_package(temp_dir.path(), "env").unwrap();
        assert_eq!(result.get("STRING_VAL").unwrap(), "hello");
        assert_eq!(result.get("INT_VAL").unwrap(), "42");
        assert_eq!(result.get("FLOAT_VAL").unwrap(), "3.14");
        assert_eq!(result.get("BOOL_VAL").unwrap(), "true");
        // null, arrays, and objects should be skipped
        assert!(!result.contains_key("NULL_VAL"));
        assert!(!result.contains_key("ARRAY_VAL"));
        assert!(!result.contains_key("OBJECT_VAL"));
    }

    #[test]
    fn test_parse_with_hooks() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "echo"
                args: ["Entering environment"]
            }
            onExit: {
                command: "cleanup.sh"
                args: ["--verbose"]
            }
        }

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 2);

        let on_enter = result.hooks.get("onEnter").unwrap();
        assert_eq!(on_enter.command, "echo");
        assert_eq!(on_enter.args, vec!["Entering environment"]);
        assert_eq!(on_enter.hook_type, HookType::OnEnter);
        assert!(on_enter.url.is_none());

        let on_exit = result.hooks.get("onExit").unwrap();
        assert_eq!(on_exit.command, "cleanup.sh");
        assert_eq!(on_exit.args, vec!["--verbose"]);
        assert_eq!(on_exit.hook_type, HookType::OnExit);
        assert!(on_exit.url.is_none());
    }

    #[test]
    fn test_parse_hooks_with_url() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "notify"
                args:    ["webhook", "start"]
                url:     "https://example.com/webhook"
            }
        }

        env: {
            API_KEY: "secret123"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 1);

        let hook = result.hooks.get("onEnter").unwrap();
        assert_eq!(hook.command, "notify");
        assert_eq!(hook.args, vec!["webhook", "start"]);
        assert_eq!(hook.url, Some("https://example.com/webhook".to_string()));
    }

    #[test]
    fn test_parse_empty_hooks() {
        let content = r#"
        package env

        hooks: {}

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 0);
    }

    #[test]
    fn test_parse_no_hooks() {
        let content = r#"
        package env

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 0);
    }

    #[test]
    fn test_parse_hooks_with_complex_args() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "docker"
                args: ["run", "-d", "--name", "test-db", "postgres:14"]
            }
            onExit: {
                command: "docker"
                args: ["stop", "test-db", "&&", "docker", "rm", "test-db"]
            }
        }

        env: {
            APP_NAME: "myapp"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        let on_enter = result.hooks.get("onEnter").unwrap();
        assert_eq!(on_enter.args.len(), 5);
        assert_eq!(on_enter.args[0], "run");
        assert_eq!(on_enter.args[4], "postgres:14");

        let on_exit = result.hooks.get("onExit").unwrap();
        assert_eq!(on_exit.args.len(), 6);
    }

    #[test]
    fn test_parse_hooks_with_environments() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "echo"
                args: ["Development environment"]
            }
        }

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
        }

        environment: {
            production: {
                DATABASE_URL: "postgres://prod.example.com/mydb"
            }
        }
        "#;
        let temp_dir = create_test_env(content);

        // Test with development (default)
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(result.hooks.len(), 1);
        assert_eq!(
            result.hooks.get("onEnter").unwrap().args[0],
            "Development environment"
        );

        // Test with production environment - hooks should remain the same
        let options = ParseOptions {
            environment: Some("production".to_string()),
            capabilities: Vec::new(),
        };
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();
        assert_eq!(result.hooks.len(), 1);
        assert_eq!(
            result.hooks.get("onEnter").unwrap().args[0],
            "Development environment"
        );
    }

    #[test]
    fn test_parse_hooks_only_on_enter() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "start-server"
                args: []
            }
        }

        env: {
            API_URL: "http://localhost:3000"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 1);
        assert!(result.hooks.contains_key("onEnter"));
        assert!(!result.hooks.contains_key("onExit"));

        let hook = result.hooks.get("onEnter").unwrap();
        assert_eq!(hook.command, "start-server");
        assert!(hook.args.is_empty());
    }

    #[test]
    fn test_parse_hooks_only_on_exit() {
        let content = r#"
        package env

        hooks: {
            onExit: {
                command: "stop-server"
                args: ["--graceful"]
            }
        }

        env: {
            API_URL: "http://localhost:3000"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 1);
        assert!(!result.hooks.contains_key("onEnter"));
        assert!(result.hooks.contains_key("onExit"));

        let hook = result.hooks.get("onExit").unwrap();
        assert_eq!(hook.command, "stop-server");
        assert_eq!(hook.args, vec!["--graceful"]);
    }

    #[test]
    fn test_parse_hooks_with_constraints() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "devenv"
                args: ["up"]
                constraints: [
                    {
                        commandExists: {
                            command: "devenv"
                        }
                    },
                    {
                        shellCommand: {
                            command: "nix"
                            args: ["--version"]
                        }
                    }
                ]
            }
            onExit: {
                command: "cleanup.sh"
                args: []
                constraints: [
                    {
                        shellCommand: {
                            command: "test"
                            args: ["-f", "/tmp/cleanup_needed"]
                        }
                    },
                    {
                        commandExists: {
                            command: "cleanup"
                        }
                    }
                ]
            }
        }

        env: {
            DATABASE_URL: "postgres://localhost/mydb"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 2);

        // Test onEnter hook constraints
        let on_enter = result.hooks.get("onEnter").unwrap();
        assert_eq!(on_enter.command, "devenv");
        assert_eq!(on_enter.args, vec!["up"]);
        assert_eq!(on_enter.constraints.len(), 2);

        // Check first constraint - command exists
        if let HookConstraint::CommandExists { command } = &on_enter.constraints[0] {
            assert_eq!(command, "devenv");
        } else {
            panic!("Expected CommandExists constraint");
        }

        // Check second constraint - shell command
        if let HookConstraint::ShellCommand { command, args } = &on_enter.constraints[1] {
            assert_eq!(command, "nix");
            assert_eq!(args.as_ref().unwrap(), &vec!["--version"]);
        } else {
            panic!("Expected ShellCommand constraint");
        }

        // Test onExit hook constraints
        let on_exit = result.hooks.get("onExit").unwrap();
        assert_eq!(on_exit.command, "cleanup.sh");
        assert!(on_exit.args.is_empty());
        assert_eq!(on_exit.constraints.len(), 2);

        // Check first constraint - shell command
        if let HookConstraint::ShellCommand { command, args } = &on_exit.constraints[0] {
            assert_eq!(command, "test");
            assert_eq!(args.as_ref().unwrap(), &vec!["-f", "/tmp/cleanup_needed"]);
        } else {
            panic!("Expected ShellCommand constraint");
        }

        // Check second constraint - command exists
        if let HookConstraint::CommandExists { command } = &on_exit.constraints[1] {
            assert_eq!(command, "cleanup");
        } else {
            panic!("Expected CommandExists constraint");
        }
    }

    #[test]
    fn test_parse_hooks_with_no_constraints() {
        let content = r#"
        package env

        hooks: {
            onEnter: {
                command: "echo"
                args: ["No constraints"]
            }
        }

        env: {
            API_KEY: "secret123"
        }
        "#;
        let temp_dir = create_test_env(content);
        let options = ParseOptions::default();
        let result =
            CueParser::eval_package_with_options(temp_dir.path(), "env", &options).unwrap();

        assert_eq!(result.hooks.len(), 1);
        let hook = result.hooks.get("onEnter").unwrap();
        assert_eq!(hook.command, "echo");
        assert_eq!(hook.args, vec!["No constraints"]);
        assert!(hook.constraints.is_empty());
    }
}
