use clap::Parser;
use std::fs;
use std::path::PathBuf;

/// A parser-based god-file splitter for Rust source.
#[derive(Debug, Parser)]
#[command(
    name = "rust-split",
    version,
    after_help = "More info: https://github.com/owebeeone/rust-split"
)]
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

    /// Split a binary crate-root file into modules each under the budget.
    Split {
        /// Rust source file to split (e.g. src/main.rs).
        file: PathBuf,

        /// Maximum LOC per output file.
        #[arg(long, default_value_t = 500)]
        max_loc: usize,

        /// Output directory (defaults to the source file's directory, in place).
        #[arg(long)]
        out: Option<PathBuf>,

        /// Treat the file as a nested library module (`foo/mod.rs`), not a bin root.
        #[arg(long)]
        module: bool,
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

        Command::Split {
            file,
            max_loc,
            out,
            module,
        } => {
            let src = fs::read_to_string(&file)?;
            let exploded = rust_split::explode(&src)?;
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("source file has no stem")?;
            let output = if module {
                rust_split::split_mod(&exploded, max_loc, stem)
            } else {
                rust_split::split_bin(&exploded, max_loc, stem)
            };
            let dir = out.unwrap_or_else(|| {
                file.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            });
            fs::create_dir_all(&dir)?;
            for file in &output.files {
                let path = dir.join(&file.path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, &file.contents)?;
            }
            for file in &output.files {
                println!("{:>5}  {}", file.loc, file.path);
            }
            if !output.still_oversized.is_empty() {
                eprintln!("still oversized (need nested split / manual extraction):");
                for item in &output.still_oversized {
                    eprintln!("  {item}");
                }
            }
            println!(
                "{} files, max {} LOC (budget {})",
                output.files.len(),
                output.max_loc(),
                max_loc
            );
            Ok(())
        }
    }
}
