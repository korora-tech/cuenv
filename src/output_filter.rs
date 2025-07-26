use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, RwLock};

pub struct OutputFilter<W: Write> {
    writer: W,
    secrets: Arc<RwLock<HashSet<String>>>,
}

impl<W: Write> OutputFilter<W> {
    pub fn new(writer: W, secrets: Arc<RwLock<HashSet<String>>>) -> Self {
        Self { writer, secrets }
    }

    fn filter_line(&self, line: &str) -> String {
        let secrets = match self.secrets.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // If the lock is poisoned, we can still read the data
                log::warn!("Secrets lock was poisoned, recovering...");
                poisoned.into_inner()
            }
        };
        let mut filtered = line.to_string();

        for secret in secrets.iter() {
            if !secret.is_empty() && filtered.contains(secret) {
                // Replace secret with asterisks of the same length
                let mask = "*".repeat(secret.len().min(8)).to_string() + "***";
                filtered = filtered.replace(secret, &mask);
            }
        }

        filtered
    }
}

impl<W: Write> Write for OutputFilter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Convert to string for filtering
        let input = String::from_utf8_lossy(buf);
        let filtered = self.filter_line(&input);

        // Write filtered output
        match self.writer.write_all(filtered.as_bytes()) {
            Ok(()) => Ok(buf.len()),
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_filter() -> Result<(), Box<dyn std::error::Error>> {
        let mut secrets = HashSet::new();
        secrets.insert("secret123".to_string());
        secrets.insert("api-key-456".to_string());

        let secrets = Arc::new(RwLock::new(secrets));
        let mut output = Vec::new();
        let mut filter = OutputFilter::new(&mut output, secrets);

        write!(filter, "This contains secret123 and api-key-456")?;

        let result = String::from_utf8(output)?;
        assert_eq!(result, "This contains *********** and ***********");

        Ok(())
    }

    #[test]
    fn test_output_filter_partial_match() -> Result<(), Box<dyn std::error::Error>> {
        let mut secrets = HashSet::new();
        secrets.insert("password".to_string());

        let secrets = Arc::new(RwLock::new(secrets));
        let mut output = Vec::new();
        let mut filter = OutputFilter::new(&mut output, secrets);

        write!(filter, "The password123 contains password")?;

        let result = String::from_utf8(output)?;
        assert_eq!(result, "The ***********123 contains ***********");

        Ok(())
    }

    #[test]
    fn test_output_filter_no_secrets() -> Result<(), Box<dyn std::error::Error>> {
        let secrets = Arc::new(RwLock::new(HashSet::new()));
        let mut output = Vec::new();
        let mut filter = OutputFilter::new(&mut output, secrets);

        write!(filter, "This has no secrets")?;

        let result = String::from_utf8(output)?;
        assert_eq!(result, "This has no secrets");

        Ok(())
    }

    #[test]
    fn test_output_filter_empty_secret() -> Result<(), Box<dyn std::error::Error>> {
        let mut secrets = HashSet::new();
        secrets.insert("".to_string());
        secrets.insert("real-secret".to_string());

        let secrets = Arc::new(RwLock::new(secrets));
        let mut output = Vec::new();
        let mut filter = OutputFilter::new(&mut output, secrets);

        write!(filter, "This has real-secret in it")?;

        let result = String::from_utf8(output)?;
        assert_eq!(result, "This has *********** in it");

        Ok(())
    }

    #[test]
    fn test_output_filter_multiline() -> Result<(), Box<dyn std::error::Error>> {
        let mut secrets = HashSet::new();
        secrets.insert("mysecret".to_string());

        let secrets = Arc::new(RwLock::new(secrets));
        let mut output = Vec::new();
        let mut filter = OutputFilter::new(&mut output, secrets);

        write!(filter, "Line 1 has mysecret\nLine 2 also has mysecret\n")?;

        let result = String::from_utf8(output)?;
        assert_eq!(
            result,
            "Line 1 has ***********\nLine 2 also has ***********\n"
        );

        Ok(())
    }

    #[test]
    fn test_output_filter_long_secret() -> Result<(), Box<dyn std::error::Error>> {
        let mut secrets = HashSet::new();
        secrets.insert("verylongsecretpasswordthatexceedseightcharacters".to_string());

        let secrets = Arc::new(RwLock::new(secrets));
        let mut output = Vec::new();
        let mut filter = OutputFilter::new(&mut output, secrets);

        write!(
            filter,
            "Secret: verylongsecretpasswordthatexceedseightcharacters"
        )?;

        let result = String::from_utf8(output)?;
        // Long secrets are masked with 8 asterisks plus "***"
        assert_eq!(result, "Secret: ***********");

        Ok(())
    }
}
