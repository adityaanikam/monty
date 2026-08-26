//! Exercises an installed, instrumented `monty` binary for PGO training.

use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
};

use monty_pool::{Pool, PoolConfig, PoolError, ReplConfig, TurnEvent, on_print_sync};

/// Runs Monty's test-case corpus through subprocess pool sessions.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let binary = find_monty_binary()?;
    println!("Training monty runtime at {}", binary.display());

    let pool = Pool::new(PoolConfig::subprocess(binary)).await?;
    let mut test_cases = test_cases()?;
    test_cases.sort();

    let mut completed = 0;
    let mut typing_errors = 0;
    for test_case in &test_cases {
        let code = fs::read_to_string(test_case)?;
        let mut session = pool
            .checkout(&ReplConfig {
                script_name: test_case
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("main.py")
                    .to_owned(),
                type_check: true,
                ..ReplConfig::default()
            })
            .await?;
        let mut on_print = on_print_sync(|_, _| {});

        match session.feed(&code, vec![], vec![], false, &mut on_print).await {
            Ok(TurnEvent::Complete(_)) => {
                completed += 1;
                session.finish().await?;
            }
            Err(PoolError::Typing(_)) => {
                typing_errors += 1;
                match session.feed(&code, vec![], vec![], true, &mut on_print).await {
                    Ok(TurnEvent::Complete(_)) => {
                        completed += 1;
                        session.finish().await?;
                    }
                    Err(PoolError::Runtime(_)) => session.finish().await?,
                    Ok(_) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(PoolError::Runtime(_)) => session.finish().await?,
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }

    println!(
        "Exercised {} test cases, {completed} completed, {typing_errors} retried without type checking",
        test_cases.len()
    );
    Ok(())
}

/// Resolves `monty` from the environment maturin prepares for PGO training.
fn find_monty_binary() -> io::Result<PathBuf> {
    let path = env::var_os("PATH").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    let executable = if cfg!(windows) { "monty.exe" } else { "monty" };
    env::split_paths(&path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.canonicalize().unwrap_or(candidate))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{executable} was not found on PATH")))
}

/// Lists the shared interpreter test cases from the workspace checkout.
fn test_cases() -> io::Result<Vec<PathBuf>> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("monty-pool should be inside the workspace crates directory")
        .join("monty/test_cases");
    fs::read_dir(directory)?
        .filter_map(|entry| match entry {
            Ok(entry) if entry.path().extension().is_some_and(|extension| extension == "py") => Some(Ok(entry.path())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}
