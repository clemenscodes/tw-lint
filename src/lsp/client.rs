use crate::cli::LintConfig;
use crate::lsp::transport::{Message, Notification, Request, Transport};
use crate::settings::tailwind_settings;
use anyhow::{anyhow, Context, Result};
use lsp_types::{PublishDiagnosticsParams, Url};
use serde_json::{json, Value};
use std::io::{BufReader, BufWriter};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub struct Client {
    child: Child,
    transport: Transport<BufReader<ChildStdout>, BufWriter<ChildStdin>>,
    settings: Value,
    diagnostics: Vec<PublishDiagnosticsParams>,
    supports_pull_diagnostics: bool,
}

impl Client {
    pub fn launch(config: &LintConfig) -> Result<Self> {
        // Honour a user-provided server and/or node: with `node` set, launch
        // `<node> <server> --stdio`; otherwise `<server> --stdio`.
        let mut command = match &config.node {
            Some(node) => {
                let mut command = Command::new(node);
                command.arg(&config.server_command);
                command
            }
            None => Command::new(&config.server_command),
        };
        let mut child = command
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning language server `{}`", config.server_command))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no server stdout"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("no server stdin"))?;
        let transport = Transport::new(BufReader::new(stdout), BufWriter::new(stdin));

        let root = std::fs::canonicalize(&config.root)
            .with_context(|| format!("canonicalizing root {}", config.root.display()))?;
        let root_uri = Url::from_directory_path(&root)
            .map_err(|_| anyhow!("root is not an absolute path: {}", root.display()))?;

        let settings = tailwind_settings(config);
        let mut client = Self {
            child,
            transport,
            settings,
            diagnostics: Vec::new(),
            supports_pull_diagnostics: false,
        };

        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": { "configuration": true, "didChangeConfiguration": {} },
                "textDocument": {
                    "publishDiagnostics": {},
                    "diagnostic": { "dynamicRegistration": false },
                    "codeAction": { "codeActionLiteralSupport": {
                        "codeActionKind": { "valueSet": ["quickfix", "source"] } } }
                }
            },
            "workspaceFolders": [ { "uri": root_uri, "name": "root" } ]
        });
        let init_result = client.request("initialize", init_params)?;
        // The server advertises pull diagnostics via `diagnosticProvider`. When
        // present we use `textDocument/diagnostic` (a synchronous request whose
        // response carries the diagnostics) instead of racing async pushes.
        client.supports_pull_diagnostics = init_result
            .get("capabilities")
            .and_then(|caps| caps.get("diagnosticProvider"))
            .is_some();
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    pub fn supports_pull_diagnostics(&self) -> bool {
        self.supports_pull_diagnostics
    }

    /// Pull diagnostics for one document (LSP 3.17 `textDocument/diagnostic`).
    /// Deterministic: the response holds the full diagnostic set, so nothing is
    /// lost to async publish timing.
    pub fn pull_diagnostics(&mut self, uri: &Url) -> Result<Vec<lsp_types::Diagnostic>> {
        let params = json!({ "textDocument": { "uri": uri } });
        let result = self.request("textDocument/diagnostic", params)?;
        let items = result
            .get("items")
            .cloned()
            .map(|value| serde_json::from_value(value).unwrap_or_default())
            .unwrap_or_default();
        Ok(items)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.transport
            .send_notification(method, params)
            .context("sending notification")
    }

    /// Close a document so the server releases it. Without this the server
    /// retains every opened document and eventually runs out of memory on a
    /// large source tree.
    pub fn close_document(&mut self, uri: &Url) -> Result<()> {
        let params = json!({ "textDocument": { "uri": uri } });
        self.notify("textDocument/didClose", params)
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let pending = self.transport.send_request(method, params)?;
        loop {
            let message = self
                .transport
                .read()
                .context("reading from server")?
                .ok_or_else(|| anyhow!("server closed the connection"))?;
            match message {
                Message::Response(response) if response.id == pending => {
                    if let Some(error) = response.error {
                        return Err(anyhow!("server error for {method}: {}", error.message));
                    }
                    return Ok(response.result.unwrap_or(Value::Null));
                }
                Message::Response(_) => continue,
                Message::Request(server_request) => self.answer(server_request)?,
                Message::Notification(note) => self.absorb(note),
            }
        }
    }

    fn answer(&mut self, request: Request) -> Result<()> {
        match request.method.as_str() {
            // The server pulls its Tailwind config from us here.
            "workspace/configuration" => {
                let items = request
                    .params
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let reply: Vec<Value> = items
                    .iter()
                    .map(|item| match item.get("section").and_then(Value::as_str) {
                        Some("tailwindCSS") => self.settings.clone(),
                        _ => Value::Null,
                    })
                    .collect();
                self.transport
                    .send_response(request.id, Value::Array(reply))?;
            }
            // Acknowledge dynamic registration / progress creation with null.
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create" => {
                self.transport.send_response(request.id, Value::Null)?;
            }
            _ => {
                self.transport.send_response(request.id, Value::Null)?;
            }
        }
        Ok(())
    }

    fn absorb(&mut self, note: Notification) {
        if note.method == "textDocument/publishDiagnostics" {
            if let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(note.params) {
                self.diagnostics.push(params);
            }
        }
    }

    pub fn take_diagnostics(&mut self) -> Vec<PublishDiagnosticsParams> {
        std::mem::take(&mut self.diagnostics)
    }

    pub fn code_actions(
        &mut self,
        uri: &Url,
        diagnostic: &lsp_types::Diagnostic,
    ) -> Result<Vec<lsp_types::CodeActionOrCommand>> {
        let params = json!({
            "textDocument": { "uri": uri },
            "range": diagnostic.range,
            "context": { "diagnostics": [diagnostic] }
        });
        let result = self.request("textDocument/codeAction", params)?;
        let actions = serde_json::from_value(result).unwrap_or_default();
        Ok(actions)
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.request("shutdown", Value::Null)?;
        self.notify("exit", Value::Null)?;
        let _ = self.child.wait();
        Ok(())
    }
}
