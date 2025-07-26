use crate::errors::{Error, Result};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Security module for validating commands, paths, and user input
pub struct SecurityValidator;

impl SecurityValidator {
    /// Validate a command against an allowlist of permitted commands
    /// Returns an error if the command is not allowed
    pub fn validate_command(command: &str, allowed_commands: &HashSet<String>) -> Result<()> {
        // Normalize the command by extracting the base command name
        let base_command = Self::extract_base_command(command);

        if allowed_commands.is_empty() {
            // If no allowlist is configured, deny all commands for safety
            return Err(Error::security(format!(
                "Command execution denied: '{}'. No commands are allowed when allowlist is empty.",
                base_command
            )));
        }

        if !allowed_commands.contains(&base_command) {
            return Err(Error::security(format!(
                "Command execution denied: '{}' is not in the allowed commands list",
                base_command
            )));
        }

        // Additional validation to prevent command injection via special characters
        if Self::contains_shell_metacharacters(command) {
            return Err(Error::security(format!(
                "Command contains potentially dangerous shell metacharacters: '{}'",
                command
            )));
        }

        Ok(())
    }

    /// Validate command arguments to prevent injection
    pub fn validate_command_args(args: &[String]) -> Result<()> {
        for arg in args {
            // Check for null bytes which can cause security issues
            if arg.contains('\0') {
                return Err(Error::security(
                    "Command argument contains null byte".to_string(),
                ));
            }

            // Check for command substitution attempts
            if Self::contains_command_substitution(arg) {
                return Err(Error::security(format!(
                    "Command argument contains potential command substitution: '{}'",
                    arg
                )));
            }
        }

        Ok(())
    }

    /// Validate a file path to prevent directory traversal attacks
    pub fn validate_path(path: &Path, allowed_base_paths: &[PathBuf]) -> Result<()> {
        // Canonicalize the path to resolve any .. or . components
        let canonical_path = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                // If path doesn't exist, manually resolve it
                let resolved = Self::resolve_path_components(path)?;
                resolved
            }
        };

        // Check if the path is within allowed base paths
        if !allowed_base_paths.is_empty() {
            let mut is_allowed = false;
            for base_path in allowed_base_paths {
                let canonical_base = base_path
                    .canonicalize()
                    .unwrap_or_else(|_| base_path.clone());
                if canonical_path.starts_with(&canonical_base) {
                    is_allowed = true;
                    break;
                }
            }

            if !is_allowed {
                return Err(Error::security(format!(
                    "Path '{}' is outside allowed directories",
                    path.display()
                )));
            }
        }

        // Additional checks for suspicious patterns
        let path_str = path.to_string_lossy();

        // Check for null bytes
        if path_str.contains('\0') {
            return Err(Error::security("Path contains null byte".to_string()));
        }

        // Check for multiple consecutive dots (more than ..)
        if path_str.contains("...") {
            return Err(Error::security(
                "Path contains suspicious dot sequences".to_string(),
            ));
        }

        Ok(())
    }

    /// Validate shell expansion to prevent injection
    pub fn validate_shell_expansion(value: &str) -> Result<()> {
        // Check for command substitution
        if Self::contains_command_substitution(value) {
            return Err(Error::security(format!(
                "Value contains potential command substitution: '{}'",
                value
            )));
        }

        // Check for dangerous environment variable expansions
        if Self::contains_dangerous_expansion(value) {
            return Err(Error::security(format!(
                "Value contains potentially dangerous expansion: '{}'",
                value
            )));
        }

        Ok(())
    }

    /// Validate CUE file content for security issues
    pub fn validate_cue_content(content: &str) -> Result<()> {
        // Check for potential code injection patterns in CUE
        let dangerous_patterns = [
            "__proto__",   // Prototype pollution
            "constructor", // Constructor injection
        ];

        for pattern in &dangerous_patterns {
            if content.contains(pattern) {
                return Err(Error::security(format!(
                    "CUE content contains potentially dangerous pattern: '{}'",
                    pattern
                )));
            }
        }

        Ok(())
    }

    /// Sanitize environment variable names
    pub fn sanitize_env_var_name(name: &str) -> Result<String> {
        // Only allow alphanumeric, underscore, and not starting with a digit
        if name.is_empty() {
            return Err(Error::security(
                "Environment variable name cannot be empty".to_string(),
            ));
        }

        let first_char = name.chars().next().unwrap();
        if !first_char.is_alphabetic() && first_char != '_' {
            return Err(Error::security(format!(
                "Environment variable name '{}' must start with a letter or underscore",
                name
            )));
        }

        for c in name.chars() {
            if !c.is_alphanumeric() && c != '_' {
                return Err(Error::security(format!(
                    "Environment variable name '{}' contains invalid character '{}'",
                    name, c
                )));
            }
        }

        Ok(name.to_string())
    }

    /// Create a default allowlist of safe commands
    pub fn default_command_allowlist() -> HashSet<String> {
        let mut allowed = HashSet::new();
        // Common safe commands - this should be configurable
        allowed.insert("echo".to_string());
        allowed.insert("printf".to_string());
        allowed.insert("pwd".to_string());
        allowed.insert("date".to_string());
        allowed.insert("true".to_string());
        allowed.insert("false".to_string());
        allowed.insert("test".to_string());
        allowed.insert("env".to_string());
        allowed.insert("printenv".to_string());
        allowed
    }

    // Helper methods

    fn extract_base_command(command: &str) -> String {
        // Extract the base command name (first word or path basename)
        let trimmed = command.trim();
        let base = trimmed.split_whitespace().next().unwrap_or("");

        // If it's a path, get the basename
        if base.contains('/') {
            Path::new(base)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(base)
                .to_string()
        } else {
            base.to_string()
        }
    }

    fn contains_shell_metacharacters(s: &str) -> bool {
        // Check for common shell metacharacters that could be dangerous
        let dangerous_chars = ['|', '&', ';', '>', '<', '`', '\n', '\r'];
        s.chars().any(|c| dangerous_chars.contains(&c))
    }

    fn contains_command_substitution(s: &str) -> bool {
        // Check for command substitution patterns
        s.contains("$(") || s.contains("${") || s.contains("`") || s.contains("$((")
    }

    fn contains_dangerous_expansion(s: &str) -> bool {
        // Check for dangerous variable expansions
        let dangerous_patterns = [
            "${IFS}",     // Can be used to manipulate command parsing
            "${PATH}",    // Can be used to hijack commands
            "${LD_",      // Dynamic linker variables
            "${BASH_ENV", // Can execute arbitrary code
            "${ENV",      // Can execute arbitrary code
        ];

        dangerous_patterns.iter().any(|pattern| s.contains(pattern))
    }

    fn resolve_path_components(path: &Path) -> Result<PathBuf> {
        let mut resolved = PathBuf::new();
        let mut depth = 0;

        for component in path.components() {
            match component {
                Component::Prefix(p) => resolved.push(p.as_os_str()),
                Component::RootDir => resolved.push("/"),
                Component::CurDir => {} // Skip .
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(Error::security(
                            "Path traversal outside of root directory".to_string(),
                        ));
                    }
                    resolved.pop();
                    depth -= 1;
                }
                Component::Normal(c) => {
                    resolved.push(c);
                    depth += 1;
                }
            }
        }

        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_command_with_allowlist() {
        let mut allowed = HashSet::new();
        allowed.insert("echo".to_string());
        allowed.insert("ls".to_string());

        // Allowed commands should pass
        assert!(SecurityValidator::validate_command("echo", &allowed).is_ok());
        assert!(SecurityValidator::validate_command("ls", &allowed).is_ok());

        // Disallowed commands should fail
        assert!(SecurityValidator::validate_command("rm", &allowed).is_err());
        assert!(SecurityValidator::validate_command("cat", &allowed).is_err());
    }

    #[test]
    fn test_validate_command_with_empty_allowlist() {
        let allowed = HashSet::new();

        // All commands should be denied with empty allowlist
        assert!(SecurityValidator::validate_command("echo", &allowed).is_err());
        assert!(SecurityValidator::validate_command("ls", &allowed).is_err());
    }

    #[test]
    fn test_validate_command_with_metacharacters() {
        let mut allowed = HashSet::new();
        allowed.insert("echo".to_string());

        // Commands with dangerous metacharacters should fail
        assert!(SecurityValidator::validate_command("echo; rm -rf /", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo | cat", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo && ls", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo `whoami`", &allowed).is_err());
    }

    #[test]
    fn test_validate_command_args() {
        // Normal args should pass
        assert!(SecurityValidator::validate_command_args(&[
            "hello".to_string(),
            "world".to_string()
        ])
        .is_ok());

        // Args with null bytes should fail
        assert!(SecurityValidator::validate_command_args(&["hello\0world".to_string()]).is_err());

        // Args with command substitution should fail
        assert!(SecurityValidator::validate_command_args(&["$(whoami)".to_string()]).is_err());
        assert!(SecurityValidator::validate_command_args(&["`date`".to_string()]).is_err());
        assert!(SecurityValidator::validate_command_args(&["${PWD}".to_string()]).is_err());
    }

    #[test]
    fn test_validate_path() {
        let allowed_paths = vec![PathBuf::from("/tmp"), PathBuf::from("/home/user")];

        // Paths within allowed directories should pass
        assert!(
            SecurityValidator::validate_path(&Path::new("/tmp/file.txt"), &allowed_paths).is_ok()
        );
        assert!(
            SecurityValidator::validate_path(&Path::new("/home/user/doc.txt"), &allowed_paths)
                .is_ok()
        );

        // Paths with null bytes should fail
        assert!(
            SecurityValidator::validate_path(&Path::new("/tmp/file\0.txt"), &allowed_paths)
                .is_err()
        );

        // Paths with suspicious patterns should fail
        assert!(
            SecurityValidator::validate_path(&Path::new("/tmp/.../file"), &allowed_paths).is_err()
        );
    }

    #[test]
    fn test_validate_shell_expansion() {
        // Normal values should pass
        assert!(SecurityValidator::validate_shell_expansion("hello world").is_ok());
        assert!(SecurityValidator::validate_shell_expansion("/path/to/file").is_ok());

        // Command substitution should fail
        assert!(SecurityValidator::validate_shell_expansion("$(whoami)").is_err());
        assert!(SecurityValidator::validate_shell_expansion("`date`").is_err());
        assert!(SecurityValidator::validate_shell_expansion("${(ls)}").is_err());

        // Dangerous expansions should fail
        assert!(SecurityValidator::validate_shell_expansion("${IFS}").is_err());
        assert!(SecurityValidator::validate_shell_expansion("${PATH}").is_err());
        assert!(SecurityValidator::validate_shell_expansion("${LD_PRELOAD}").is_err());
    }

    #[test]
    fn test_validate_cue_content() {
        // Normal CUE content should pass
        assert!(SecurityValidator::validate_cue_content("env: { FOO: \"bar\" }").is_ok());

        // Content with dangerous patterns should fail
        assert!(SecurityValidator::validate_cue_content("__proto__: {}").is_err());
        assert!(SecurityValidator::validate_cue_content("constructor: {}").is_err());
    }

    #[test]
    fn test_sanitize_env_var_name() {
        // Valid names should pass
        assert_eq!(
            SecurityValidator::sanitize_env_var_name("FOO").unwrap(),
            "FOO"
        );
        assert_eq!(
            SecurityValidator::sanitize_env_var_name("_BAR").unwrap(),
            "_BAR"
        );
        assert_eq!(
            SecurityValidator::sanitize_env_var_name("FOO_BAR_123").unwrap(),
            "FOO_BAR_123"
        );

        // Invalid names should fail
        assert!(SecurityValidator::sanitize_env_var_name("").is_err());
        assert!(SecurityValidator::sanitize_env_var_name("123FOO").is_err());
        assert!(SecurityValidator::sanitize_env_var_name("FOO-BAR").is_err());
        assert!(SecurityValidator::sanitize_env_var_name("FOO BAR").is_err());
        assert!(SecurityValidator::sanitize_env_var_name("FOO$BAR").is_err());
    }

    #[test]
    fn test_extract_base_command() {
        assert_eq!(
            SecurityValidator::extract_base_command("echo hello"),
            "echo"
        );
        assert_eq!(SecurityValidator::extract_base_command("/bin/ls -la"), "ls");
        assert_eq!(
            SecurityValidator::extract_base_command("/usr/bin/echo"),
            "echo"
        );
        assert_eq!(SecurityValidator::extract_base_command("  pwd  "), "pwd");
    }

    #[test]
    fn test_path_traversal_detection() {
        let allowed_paths = vec![PathBuf::from("/tmp"), PathBuf::from("/home/user")];

        // Test path traversal attempts
        assert!(
            SecurityValidator::validate_path(&Path::new("/tmp/../etc/passwd"), &allowed_paths)
                .is_err()
        );
        assert!(
            SecurityValidator::validate_path(&Path::new("/tmp/../../root"), &allowed_paths)
                .is_err()
        );
        assert!(
            SecurityValidator::validate_path(&Path::new("/tmp/.//../etc"), &allowed_paths).is_err()
        );
    }

    #[test]
    fn test_command_injection_patterns() {
        let mut allowed = HashSet::new();
        allowed.insert("echo".to_string());

        // Test various injection attempts
        assert!(SecurityValidator::validate_command("echo; rm -rf /", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo && whoami", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo || id", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo | tee /etc/passwd", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo > /etc/passwd", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo < /etc/shadow", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo\nrm -rf /", &allowed).is_err());
        assert!(SecurityValidator::validate_command("echo\rrm -rf /", &allowed).is_err());
    }

    #[test]
    fn test_validate_cue_dangerous_patterns() {
        // Test prototype pollution patterns
        assert!(SecurityValidator::validate_cue_content(
            r#"
            env: {
                "__proto__": {"isAdmin": true}
            }
        "#
        )
        .is_err());

        // Test constructor injection
        assert!(SecurityValidator::validate_cue_content(
            r#"
            env: {
                "constructor": {"prototype": {"isAdmin": true}}
            }
        "#
        )
        .is_err());

        // Normal content should pass
        assert!(SecurityValidator::validate_cue_content(
            r#"
            env: {
                DATABASE_URL: "postgres://localhost/mydb"
                API_KEY: "secret123"
            }
        "#
        )
        .is_ok());
    }

    #[test]
    fn test_shell_expansion_edge_cases() {
        // Test arithmetic expansion
        assert!(SecurityValidator::validate_shell_expansion("$((2+2))").is_err());
        assert!(SecurityValidator::validate_shell_expansion("$(($(whoami)))").is_err());

        // Test process substitution (bash/zsh specific)
        assert!(SecurityValidator::validate_shell_expansion("<(whoami)").is_ok()); // Not caught as it's shell-specific

        // Test variable expansion edge cases
        assert!(SecurityValidator::validate_shell_expansion("${PATH:+malicious}").is_err());
        assert!(SecurityValidator::validate_shell_expansion("${IFS:=;}").is_err());
    }

    #[test]
    fn test_env_var_name_unicode() {
        // Test unicode characters (should fail)
        assert!(SecurityValidator::sanitize_env_var_name("FOO_БАР").is_err());
        assert!(SecurityValidator::sanitize_env_var_name("FOO_🚀").is_err());
        assert!(SecurityValidator::sanitize_env_var_name("FOO\u{200B}BAR").is_err()); // Zero-width space

        // Test valid names with numbers
        assert_eq!(
            SecurityValidator::sanitize_env_var_name("FOO123").unwrap(),
            "FOO123"
        );
        assert_eq!(
            SecurityValidator::sanitize_env_var_name("_123FOO").unwrap(),
            "_123FOO"
        );
    }
}
