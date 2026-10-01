use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
};

use chunk_build::migrations::{self, Renames};
use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct Options {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a migration for the schema changes since the last snapshot.
    New {
        name: String,
        #[arg(default_value = ".")]
        project: PathBuf,
        /// Answers a rename question: TABLE.OLD=NEW. Without a terminal, unanswered fields aren't renames.
        #[arg(long = "rename", value_name = "TABLE.OLD=NEW")]
        renames: Vec<String>,
    },
    /// Write the migration that drops the old shape of expand migration NUMBER.
    Finish {
        number: String,
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Check the journal for conflicts, such as migrations from different branches.
    Check {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Replace finished history with one baseline snapshot.
    Squash {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Record the current hash of migration NUMBER after editing it. Only for migrations never deployed.
    Rehash {
        number: String,
        #[arg(default_value = ".")]
        project: PathBuf,
    },
}

pub(crate) fn run(options: Options) -> io::Result<()> {
    match options.command {
        Command::New { name, project, renames } => create(&project, &name, &renames),
        Command::Finish { number, project } => {
            let id = migrations::finish(&project, &number)?;
            cliclack::log::success(format!("Wrote {id}; deploy it once no running deployment needs the old shape"))
        }
        Command::Check { project } => {
            migrations::check(&project)?;
            cliclack::log::success("Migration journal is consistent")
        }
        Command::Squash { project } => {
            let id = migrations::squash(&project)?;
            cliclack::log::success(format!("Squashed finished migrations into {id}"))
        }
        Command::Rehash { number, project } => {
            let id = migrations::rehash(&project, &number)?;
            cliclack::log::success(format!("Recorded the hash of {id}"))
        }
    }
}

fn create(project: &Path, name: &str, answers: &[String]) -> io::Result<()> {
    let pending = migrations::pending(project)?;
    let mut renames = parse(answers)?;
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        ask(pending.changes(), &mut renames)?;
    }
    let id = migrations::create(pending, name, &renames)?;
    cliclack::log::success(format!("Wrote server/migrations/{id}"))
}

fn parse(answers: &[String]) -> io::Result<Renames> {
    let mut renames = Renames::new();
    for answer in answers {
        let (table, old, new) = answer
            .split_once('.')
            .and_then(|(table, rest)| rest.split_once('=').map(|(old, new)| (table, old, new)))
            .ok_or_else(|| io::Error::other(format!("--rename {answer}: expected TABLE.OLD=NEW")))?;
        renames.entry(table.into()).or_default().insert(old.into(), new.into());
    }
    Ok(renames)
}

/// Asks, for each removed field that isn't answered yet, whether it was renamed to one of the table's new fields.
fn ask(
    changes: &std::collections::BTreeMap<String, chunk_contract::MigrationTable>,
    renames: &mut Renames,
) -> io::Result<()> {
    for (table, change) in changes {
        let answered = renames.entry(table.clone()).or_default();
        for old in change.removed.iter().filter(|field| !change.added.contains(field)) {
            let candidates: Vec<_> = change
                .added
                .iter()
                .filter(|field| !change.removed.contains(field) && !answered.values().any(|new| new == *field))
                .collect();
            if answered.contains_key(old) || candidates.is_empty() {
                continue;
            }
            let mut prompt =
                cliclack::select(format!("Was {table}.{old} renamed?")).item(None, "No, it was removed", "");
            for field in candidates {
                prompt = prompt.item(Some(field.clone()), format!("Renamed to {field}"), "");
            }
            if let Some(new) = prompt.interact()? {
                answered.insert(old.clone(), new);
            }
        }
    }
    renames.retain(|_, fields| !fields.is_empty());
    Ok(())
}
