//! CLI subcommand implementations. Each module owns one command and
//! returns `anyhow::Result<()>` so errors bubble up to `main` for unified
//! reporting.

pub mod deploy;
pub mod keys;
pub mod logs;
pub mod status;
pub mod validate;

/// Output mode shared by every command. Selected once via the global
/// `--json` flag in `main` and threaded into every command function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
}

/// The JSON print every command uses. In mock mode the object is wrapped
/// with `"mock": true` so scripts parsing the output can tell mock data
/// from a real deployment's.
pub fn print_json(value: serde_json::Value, mock: bool) -> Result<(), serde_json::Error> {
    let out = if mock {
        let mut obj = value.as_object().cloned().unwrap_or_default();
        obj.insert("mock".into(), serde_json::json!(true));
        serde_json::Value::Object(obj)
    } else {
        value
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
