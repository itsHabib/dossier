use anyhow::{anyhow, bail, Context, Result};
use rmcp::{transport::stdio, ServiceExt};

use dossier::server::MeshService;
use dossier::store::FsStore;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let Some((_program, rest)) = args.split_first() else {
        unreachable!("argv always contains the program name");
    };
    let Some((cmd, rest)) = rest.split_first() else {
        print_usage();
        bail!("no command given");
    };
    match cmd.as_str() {
        "serve" => run_serve(rest).await,
        "help" | "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown command: {other}");
        }
    }
}

async fn run_serve(args: &[String]) -> Result<()> {
    let mut iter = args.iter();
    let mut corpus: Option<String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--corpus" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow!("--corpus requires a path"))?;
                corpus = Some(value.clone());
            }
            other => bail!("unknown serve flag: {other}"),
        }
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
        "dossier — project memory for the solo developer\n\n\
         usage: dossier <command> [args...]\n\n\
         commands:\n  \
           serve --corpus <path>   run the MCP server over stdio against the corpus at <path>\n  \
                                   register with: claude mcp add dossier -- <path-to-binary> serve --corpus <corpus>\n\n\
         The corpus is any directory containing a .dossier/ marker. See LAYOUT.md\n\
         for the on-disk format."
    );
}
