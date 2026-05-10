use std::env;

use anyhow::{bail, Context, Result};
use rmcp::{transport::stdio, ServiceExt};

use dossier::server::MeshService;
use dossier::store::FsStore;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        bail!("no command given");
    }
    match args[1].as_str() {
        "serve" => run_serve(&args[2..]).await,
        "help" | "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        cmd => {
            print_usage();
            bail!("unknown command: {cmd}");
        }
    }
}

async fn run_serve(args: &[String]) -> Result<()> {
    let mut corpus: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--corpus" => {
                i += 1;
                if i >= args.len() {
                    bail!("--corpus requires a path");
                }
                corpus = Some(args[i].clone());
            }
            other => bail!("unknown serve flag: {other}"),
        }
        i += 1;
    }
    let corpus = corpus.context("--corpus is required (the mcp server has no cwd context)")?;
    let store = FsStore::open(&corpus)?;
    let service = MeshService::new(store);
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

fn print_usage() {
    eprintln!(
        "dossier-mesh — reference Agent Project Protocol server\n\n\
         usage: dossier-mesh <command> [args...]\n\n\
         commands:\n  \
           serve --corpus <path>   run the MCP server over stdio against the corpus at <path>\n  \
                                   register with: claude mcp add dossier -- <path-to-binary> serve --corpus <corpus>\n\n\
         The corpus is any directory containing a .dossier/ marker. See LAYOUT.md\n\
         for the on-disk format."
    );
}
