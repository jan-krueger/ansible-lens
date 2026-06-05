//! ansible-lens-lsp
//!
//! A standalone Language Server that provides Go-to-Definition for nested
//! Ansible/Jinja2 variables. Communicates over stdio using LSP. Launched by the
//! Zed extension host (see `editors/zed/`), but works with any LSP client.

mod backend;
mod config;
mod flatten;
mod index;
mod jinja;
mod precedence;

use backend::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
