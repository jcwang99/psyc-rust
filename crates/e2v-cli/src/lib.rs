use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use e2v_core::{MetadataSearchQuery, RepositoryFacade};

#[derive(Debug, Parser)]
#[command(name = "e2v")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Branch {
        #[command(subcommand)]
        command: BranchCommand,
        #[arg(long)]
        repo: PathBuf,
    },
    Search {
        query: String,
        #[arg(long)]
        repo: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum BranchCommand {
    List,
    Create { name: String },
    Checkout { name: String },
    Delete { name: String },
}

pub fn run_cli_for_test<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    execute(cli)
}

pub fn run_from_env() -> Result<()> {
    let output = execute(Cli::parse())?;
    print!("{output}");
    Ok(())
}

fn execute(cli: Cli) -> Result<String> {
    let facade = RepositoryFacade::new();
    match cli.command {
        Command::Branch { command, repo } => match command {
            BranchCommand::List => {
                let branches = facade.list_branches(repo)?;
                Ok(branches
                    .into_iter()
                    .map(|branch| {
                        let marker = if branch.is_current { "*" } else { " " };
                        match branch.head_snapshot_id {
                            Some(head) => format!("{marker} {} {head}\n", branch.name),
                            None => format!("{marker} {}\n", branch.name),
                        }
                    })
                    .collect())
            }
            BranchCommand::Create { name } => {
                let branch = facade.create_branch(repo, &name)?;
                Ok(format!("created branch {}\n", branch.name))
            }
            BranchCommand::Checkout { name } => {
                let state = facade.checkout_branch(repo, &name)?;
                Ok(format!("checked out {}\n", state.branch.name))
            }
            BranchCommand::Delete { name } => {
                facade.delete_branch(repo, &name)?;
                Ok(format!("deleted branch {name}\n"))
            }
        },
        Command::Search { query, repo } => {
            let results = facade.search_filenames(&repo, &query)?;
            if !results.is_empty() {
                return Ok(results
                    .into_iter()
                    .map(|result| format!("{}\n", result.path))
                    .collect());
            }
            let metadata = facade.search_metadata(
                &repo,
                MetadataSearchQuery {
                    extension: Some(query),
                    path_prefix: None,
                    min_size: None,
                    max_size: None,
                },
            )?;
            Ok(metadata
                .into_iter()
                .map(|result| format!("{}\n", result.path))
                .collect())
        }
    }
}
