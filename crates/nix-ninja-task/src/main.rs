use anyhow::{anyhow, Result};
use clap::Parser;
use harmonia_store_core::store_path::StoreDir;
use nix_ninja_task::derived_file::{create_symlinks, DerivedFile};
use nix_ninja_task::patchelf;
use nix_varlink_client::{dump_nar, VarlinkClient};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(author, disable_version_flag = true)]
pub struct Cli {
    /// Specify the Nix store directory.
    #[arg(long = "store-dir", env = "NIX_STORE", default_value = "/nix/store")]
    pub store_dir: StoreDir,

    /// Directory prefix to recreate sources via symlinks.
    #[arg(long = "build-dir", default_value = "/build/source/build")]
    pub build_dir: PathBuf,

    /// Optional build target description.
    #[arg(long)]
    pub description: Option<String>,

    // Encoded derived files to prepare the source directory.
    #[arg(long, env = "NIX_NINJA_INPUTS")]
    pub inputs: String,

    // Encoded derived files that build outputs should be copied to.
    #[arg(long, env = "NIX_NINJA_OUTPUTS")]
    pub outputs: String,

    // Command to run.
    pub cmdline: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    fs::create_dir_all(&cli.build_dir)?;
    std::env::set_current_dir(&cli.build_dir)?;

    let mut inputs = Vec::new();
    for encoded in cli.inputs.split_whitespace() {
        // println!("Processing input {}", encoded);
        let input = DerivedFile::from_encoded(&cli.store_dir, encoded)?;
        inputs.push(input);
    }

    let mut outputs = Vec::new();
    for encoded in cli.outputs.split_whitespace() {
        // println!("Processing output {}", encoded);
        let output = DerivedFile::from_encoded(&cli.store_dir, encoded)?;
        outputs.push(output);
    }

    // The source directory of the derivation needs to have all build inputs
    // symlinked while preserving the original directory hierarchy of the
    // sources. This ensures relative includes and other path-dependent
    // references remain valid.
    create_symlinks(&cli.build_dir, &cli.store_dir, inputs, false)?;
    println!(
        "nix-ninja-task: Setup source directory in {}",
        cli.build_dir.display()
    );

    // Outputs are written to the same directory structure as the build
    // directory because if the output is a shared library the filename must
    // match the soname and it must be in a directory to add to the linking
    // binary's RUNPATH.
    create_output_dirs(&outputs)?;

    if let Some(desc) = cli.description {
        println!("nix-ninja-task: {desc}");
    }

    // Spawn cmdline process via sh like ninja upstream does.
    println!("nix-ninja-task: Running: /bin/sh -c \"{}\"", cli.cmdline);
    let exit_code = spawn_process(&cli.cmdline)?;
    if exit_code != 0 {
        println!("nix-ninja-task: Failed with exit code {exit_code}");
        std::process::exit(exit_code);
    }

    // Fix ELF RPATH to ensure it's linked against /nix/store paths rather
    // than relative path binaries in the build dir.
    patchelf::fix_rpaths(cli.store_dir.to_path(), &outputs)?;

    // Submit outputs back to the store. Outputs are physically created in the
    // build directory and then handed over because ninja build rules can have
    // implicit outputs that we have no way of knowing — e.g. a custom command
    // that doesn't leverage the `$out` implicit variable in the ninja
    // evaluation context.
    if let Some(mut client) = VarlinkClient::connect_from_env()? {
        println!(
            "nix-ninja-task: Finished! Submitting {} outputs via builder-rpc-v1",
            outputs.len(),
        );
        submit_outputs_via_varlink(&mut client, &cli.store_dir, &outputs)?;
    } else {
        println!(
            "nix-ninja-task: Finished! Copying {} build outputs to derivation output paths",
            outputs.len(),
        );
        copy_outputs_to_placeholders(&cli.store_dir, &outputs)?;
    }

    Ok(())
}

fn copy_outputs_to_placeholders(store_dir: &StoreDir, outputs: &[DerivedFile]) -> Result<()> {
    for output in outputs {
        let target_path = output.absolute_path(store_dir);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&output.build_path, &target_path)?;
    }
    Ok(())
}

fn submit_outputs_via_varlink(
    client: &mut VarlinkClient,
    store_dir: &StoreDir,
    outputs: &[DerivedFile],
) -> Result<()> {
    // Stage each output as a directory whose contents match what consumers
    // would have seen at the placeholder path: a file at `<rel_path>`. We NAR
    // that staging directory and submit it.
    let staging_root = std::env::var("NIX_BUILD_TOP")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("nix-ninja-task-outputs");
    fs::create_dir_all(&staging_root)?;

    for output in outputs {
        let name = output
            .output_name
            .as_deref()
            .ok_or_else(|| anyhow!("output {} has no output name", output.build_path.display()))?;
        let rel_path = output
            .rel_path
            .as_ref()
            .ok_or_else(|| anyhow!("output {name} has no rel_path"))?;
        ensure_safe_relative(rel_path, name)?;

        let staging = staging_root.join(name);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        let dest = staging.join(rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&output.build_path, &dest)?;

        let store_path =
            client.add_to_store_nar(store_dir, name, |sink| dump_nar(&staging, sink))?;
        client.submit_output(store_dir, name, &store_path)?;

        println!(
            "nix-ninja-task: Submitted output {name} -> {}",
            store_dir.display(&store_path),
        );
    }

    Ok(())
}

/// Reject relative paths that would escape the staging directory after
/// `Path::join` (absolute paths replace the base; `..` walks above it).
fn ensure_safe_relative(p: &Path, name: &str) -> Result<()> {
    if p.is_absolute() {
        return Err(anyhow!(
            "output {name} has absolute rel_path: {}",
            p.display()
        ));
    }
    for component in p.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(anyhow!(
                    "output {name} has unsafe rel_path component in {}",
                    p.display()
                ));
            }
        }
    }
    Ok(())
}

fn create_output_dirs(outputs: &Vec<DerivedFile>) -> Result<()> {
    let mut dirs: Vec<&std::path::Path> = Vec::new();
    for output in outputs {
        if let Some(parent) = output.build_path.parent() {
            if dirs.contains(&parent) {
                continue;
            }
            std::fs::create_dir_all(parent)?;
            dirs.push(parent);
        }
    }
    Ok(())
}

fn spawn_process(cmdline: &str) -> Result<i32> {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", cmdline])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .envs(env::vars());

    let output = cmd.status()?;
    Ok(output.code().unwrap_or(1))
}
