use std::{
    fs, io,
    path::{Path, PathBuf},
};

use chunk_contract::MigrationKind;
use oxc_allocator::Allocator;
use oxc_ast::ast::Statement;
use oxc_parser::Parser;
use oxc_span::SourceType;

use super::bundle::MigrationSource;
use crate::{migrations::Journal, project::Inventory};

/// One compilation's private copy of the generated declarations and the journal's migration sources, removed on drop.
pub(super) struct Stage {
    _directory: tempfile::TempDir,
    pub chunk: PathBuf,
    pub migrations: Vec<MigrationSource>,
}

/// Stages `journal`'s expand migrations after checking that they import only `#chunk`. Only this directory is
/// type-checked and bundled for them, so no other compilation can change what they are compiled against.
pub(super) fn stage(project: &Path, inventory: &Inventory, journal: &Journal) -> io::Result<Stage> {
    let directory = tempfile::Builder::new().prefix("compile-").tempdir_in(project.join(".chunk"))?;
    for (name, content) in crate::sdk::generated(inventory, journal)? {
        fs::write(directory.path().join(name), content)?;
    }
    fs::create_dir(directory.path().join("migrations"))?;
    let mut migrations = Vec::new();
    for (entry, code) in journal.entries.iter().zip(&journal.sources) {
        if entry.kind == MigrationKind::Expand {
            require_chunk_imports(&entry.id, code)?;
            let path = directory.path().join("migrations").join(format!("{}.ts", entry.id));
            fs::write(&path, code)?;
            migrations.push(MigrationSource { id: entry.id.clone(), path, code: code.clone() });
        }
    }
    Ok(Stage { chunk: directory.path().join("index.ts"), _directory: directory, migrations })
}

fn require_chunk_imports(id: &str, code: &str) -> io::Result<()> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::ts()).parse();
    if let Some(error) = parsed.diagnostics.first() {
        return Err(io::Error::other(format!("server/migrations/{id}.ts: {error}")));
    }
    for statement in &parsed.program.body {
        let source = match statement {
            Statement::ImportDeclaration(declaration) => Some(&declaration.source),
            Statement::ExportAllDeclaration(declaration) => Some(&declaration.source),
            Statement::ExportFromDeclaration(declaration) => Some(&declaration.source),
            _ => None,
        };
        if let Some(source) = source.filter(|source| source.value != "#chunk") {
            return Err(io::Error::other(format!(
                "server/migrations/{id}.ts imports {}; migrations may import only from #chunk",
                source.value
            )));
        }
    }
    Ok(())
}
