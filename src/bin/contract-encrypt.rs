//! Encrypt a plaintext OpenAPI spec to an age passphrase-encrypted blob.
//!
//! Used by `contract/regen.sh`. Reads the passphrase from stdin (never an
//! argv, never an env var printed in logs) and encrypts the file at `argv[1]`
//! to `argv[2]`.

use std::io::{self, Read, Write};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: contract-encrypt <input-file> <output-file>");
        eprintln!("  (passphrase is read from stdin)");
        std::process::exit(2);
    }
    let input_path = &args[1];
    let output_path = &args[2];

    let mut passphrase = String::new();
    io::stdin().read_to_string(&mut passphrase)?;
    let passphrase = passphrase.trim_end_matches(['\n', '\r']).to_string();
    if passphrase.is_empty() {
        anyhow::bail!("passphrase is empty");
    }

    let plaintext = std::fs::read(input_path)?;

    let encryptor = age::Encryptor::with_user_passphrase(
        secrecy::SecretString::new(passphrase),
    );
    let output_file = std::fs::File::create(output_path)?;
    let mut output = encryptor.wrap_output(output_file)?;
    output.write_all(&plaintext)?;
    output.finish()?;

    eprintln!("Encrypted {} -> {}", input_path, output_path);
    Ok(())
}
