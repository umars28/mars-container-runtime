use std::fs;
use std::io::Write;

use oci_spec::runtime::Spec;

use crate::cli::SpecArgs;
use crate::error::{Error, IoContext, Result};

pub fn run(args: &SpecArgs, rootless: bool) -> Result<()> {
    if rootless {
        return Err(Error::Unimplemented("spec --rootless"));
    }

    fs::create_dir_all(&args.bundle).ctx(format!("create bundle dir {}", args.bundle.display()))?;

    let path = args.bundle.join("config.json");
    if path.exists() {
        return Err(Error::Io {
            context: format!("{} already exists", path.display()),
            source: std::io::Error::new(std::io::ErrorKind::AlreadyExists, "refusing to overwrite"),
        });
    }

    let mut spec = Spec::default();
    spec.set_version(crate::OCI_VERSION.to_string());
    spec.set_hostname(Some("mars".to_string()));

    let mut json = serde_json::to_string_pretty(&spec)?;
    json.push('\n');

    let mut file = fs::File::create(&path).ctx(format!("create {}", path.display()))?;
    file.write_all(json.as_bytes())
        .ctx(format!("write {}", path.display()))?;

    Ok(())
}
