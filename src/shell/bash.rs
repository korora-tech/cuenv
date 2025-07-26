use super::{escape_bash_like, Shell};

pub struct BashShell;

impl Shell for BashShell {
    fn hook(&self) -> String {
        r#"_cuenv_hook() {
  local previous_exit_status=$?
  trap -- '' SIGINT
  eval "$(cuenv hook bash)"
  trap - SIGINT
  return $previous_exit_status
}

if [[ ";${PROMPT_COMMAND[*]:-};" != *";_cuenv_hook;"* ]]; then
  if [[ "$(declare -p PROMPT_COMMAND 2>&1)" == "declare -a"* ]]; then
    PROMPT_COMMAND+=(_cuenv_hook)
  else
    PROMPT_COMMAND="_cuenv_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  fi
fi"#
        .to_string()
    }

    fn export(&self, key: &str, value: &str) -> String {
        // Note: Key validation should be done by the caller to handle errors properly
        // This is just the formatting function
        format!("export {key}={}", self.escape(value))
    }

    fn unset(&self, key: &str) -> String {
        format!("unset {key}")
    }

    fn escape(&self, s: &str) -> String {
        escape_bash_like(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bash_export() {
        let shell = BashShell;
        assert_eq!(shell.export("FOO", "bar"), "export FOO=bar");
        assert_eq!(shell.export("FOO", "bar baz"), "export FOO='bar baz'");
        assert_eq!(shell.export("FOO", "it's"), "export FOO='it'\"'\"'s'");
    }

    #[test]
    fn test_bash_unset() {
        let shell = BashShell;
        assert_eq!(shell.unset("FOO"), "unset FOO");
    }

    #[test]
    fn test_bash_hook() {
        let shell = BashShell;
        let hook = shell.hook();
        assert!(hook.contains("_cuenv_hook"));
        assert!(hook.contains("PROMPT_COMMAND"));
        assert!(hook.contains("declare -a"));
    }
}
