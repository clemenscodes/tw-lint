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

        let mut client = Self {
            child,
            transport,
            settings: tailwind_settings(config),
            diagnostics: Vec::new(),
        };

        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": { "configuration": true, "didChangeConfiguration": {} },
                "textDocument": {
                    "publishDiagnostics": {},
                    "codeAction": { "codeActionLiteralSupport": {
                        "codeActionKind": { "valueSet": ["quickfix", "source"] } } }
                }
            },
            "workspaceFolders": [ { "uri": root_uri, "name": "root" } ]
        });
        client.request("initialize", init_params)?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.transport
            .send_notification(method, params)
            .context("sending notification")
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

    pub fn shutdown(mut self) -> Result<()> {
        self.request("shutdown", Value::Null)?;
        self.notify("exit", Value::Null)?;
        let _ = self.child.wait();
        Ok(())
    }
}
