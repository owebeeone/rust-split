use clap::Parser;
use std::fs;
use std::path::PathBuf;

/// A parser-based god-file splitter for Rust source.
#[derive(Debug, Parser)]
#[command(name = "rust-split", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Explode a Rust source file into top-level item chunks.
    Explode {
        /// Rust source file to explode.
        file: PathBuf,

        /// Output directory for chunk files and manifest.toml.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rust-split: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Explode { file, out } => {
            let src = fs::read_to_string(&file)?;
            let exploded = rust_split::explode(&src)?;
            let joined = exploded
                .chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>();
            if joined != src {
                return Err("chunk tiling invariant failed".into());
            }

            fs::create_dir_all(&out)?;
            for (index, chunk) in exploded.chunks.iter().enumerate() {
                fs::write(out.join(format!("chunk-{index:03}.rs")), &chunk.text)?;
            }
            fs::write(
                out.join("manifest.toml"),
                rust_split::manifest_toml(&exploded.manifest)?,
            )?;
            Ok(())
        }
    }
}
