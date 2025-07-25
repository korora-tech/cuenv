use crate::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

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
        // Only allow loading the "env" package
        if package_name != "env" {
            return Err(Error::configuration(format!(
                "Only 'env' package is supported, got '{package_name}'. Please ensure your .cue files use 'package env'"
            )));
        }
        let c_dir = match CString::new(dir.to_string_lossy().as_ref()) {
            Ok(s) => s,
            Err(e) => {
                return Err(Error::ffi(
                    "cue_eval_package",
                    format!("failed to create C string from directory path: {e}"),
                ));
            }
        };

        let c_package = match CString::new(package_name) {
            Ok(s) => s,
            Err(e) => {
                return Err(Error::ffi(
                    "cue_eval_package",
                    format!("failed to create C string from package name: {e}"),
                ));
            }
        };

        let result_ptr = unsafe { cue_eval_package(c_dir.as_ptr(), c_package.as_ptr()) };

        if result_ptr.is_null() {
            return Err(Error::cue_parse(dir, "CUE parser returned null pointer"));
        }

        let result_cstr = unsafe { CStr::from_ptr(result_ptr) };
        let result_str = match result_cstr.to_str() {
            Ok(s) => s,
            Err(e) => {
                unsafe { cue_free_string(result_ptr) };
                return Err(Error::ffi(
                    "cue_eval_package",
                    format!("failed to convert C string to Rust string: {e}"),
                ));
            }
        };

        let parse_result = if result_str.is_empty() {
            ParseResult {
                variables: HashMap::new(),
                commands: HashMap::new(),
                tasks: HashMap::new(),
                hooks: HashMap::new(),
            }
        } else {
            // Parse JSON result
            let json_value: serde_json::Value = match serde_json::from_str(result_str) {
                Ok(v) => v,
                Err(e) => {
                    unsafe { cue_free_string(result_ptr) };
                    return Err(Error::Json {
                        message: "failed to parse JSON result from CUE parser".to_string(),
                        source: e,
                    });
                }
            };

            // Check if it's an error response
            if let serde_json::Value::Object(ref map) = json_value {
                if let Some(serde_json::Value::String(error)) = map.get("error") {
                    unsafe { cue_free_string(result_ptr) };
                    return Err(Error::cue_parse(dir, error.clone()));
                }
            }

            // Deserialize into structured result
            let cue_result: CueParseResult = match serde_json::from_value(json_value) {
                Ok(r) => r,
                Err(e) => {
                    unsafe { cue_free_string(result_ptr) };
                    return Err(Error::Json {
                        message: "failed to parse CUE result structure".to_string(),
                        source: e,
                    });
                }
            };

            match Self::build_parse_result(cue_result, options) {
                Ok(r) => r,
                Err(e) => {
                    unsafe { cue_free_string(result_ptr) };
                    return Err(e);
                }
            }
        };

        // Free the C string
        unsafe { cue_free_string(result_ptr) };

        Ok(parse_result)
    }

    fn build_parse_result(
        cue_result: CueParseResult,
        options: &ParseOptions,
    ) -> Result<ParseResult> {
        let mut final_vars = HashMap::new();

        // Start with base variables
        for (key, val) in &cue_result.variables {
            // Check capability filter
            let should_include = if let Some(metadata) = cue_result.metadata.get(key) {
                if let Some(cap) = &metadata.capability {
                    // Variable has a capability tag, only include if it matches the filter
                    options.capabilities.is_empty() || options.capabilities.contains(cap)
                } else {
                    // No capability tag means always include
                    true
                }
            } else {
                // No metadata means no capability tag, always include
                true
            };

            if should_include {
                if let Some(str_val) = Self::value_to_string(val) {
                    final_vars.insert(key.clone(), str_val);
                }
            }
        }

        // Apply environment-specific overrides
        if let Some(env_name) = &options.environment {
            if let Some(env_vars) = cue_result.environments.get(env_name) {
                for (key, val) in env_vars {
                    // For environment overrides, we still need to check capabilities
                    // if they were specified in the base variable
                    let should_include = if let Some(metadata) = cue_result.metadata.get(key) {
                        if let Some(cap) = &metadata.capability {
                            // Variable has a capability tag, only include if it matches the filter
                            options.capabilities.is_empty() || options.capabilities.contains(cap)
                        } else {
                            // No capability tag means always include
                            true
                        }
                    } else {
                        // No metadata means no capability tag, always include
                        true
                    };

                    if should_include {
                        if let Some(str_val) = Self::value_to_string(val) {
                            final_vars.insert(key.clone(), str_val);
                        }
                    }
                }
            }
        }

        // Process hooks
        let mut hooks = HashMap::new();
        if let Some(hooks_config) = cue_result.hooks {
            if let Some(mut on_enter) = hooks_config.on_enter {
                on_enter.hook_type = HookType::OnEnter;
                hooks.insert("onEnter".to_string(), on_enter);
            }
            if let Some(mut on_exit) = hooks_config.on_exit {
                on_exit.hook_type = HookType::OnExit;
                hooks.insert("onExit".to_string(), on_exit);
            }
        }

        Ok(ParseResult {
            variables: final_vars,
            commands: cue_result.commands,
            tasks: cue_result.tasks,
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

#[cfg(test)]
mod tests {
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
            "Error message was: {}",
            err_msg
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
