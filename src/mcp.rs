//! The MCP server, and the only module that knows `rmcp` (D4).
//!
//! No `rmcp` type leaves this file. The rest of Proxenos implements [`Service`], and this module
//! adds what the library does not do itself: it stops running calls the moment standard input
//! ends, reports a failure to read or write as a failure, hands every call a cancellation token,
//! and answers a call whose handler panicked.

use std::borrow::Cow;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::service::{QuitReason, RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// How long the server may take, from the end of its input, to finish every running call and
/// write what is left of its output. Past it, serving has failed.
const STOP_WITHIN: Duration = Duration::from_secs(10);

/// The size of the in-memory pipes between rmcp and the real standard input and output.
const PIPE_BYTES: usize = 64 * 1024;

/// A tool as the server lists it.
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// A JSON Schema object, sent unchanged.
    pub input_schema: Map<String, Value>,
}

/// What a tool call produced: text for the model, and whether it is a failure of the tool.
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

/// The call named no tool this server has.
pub struct UnknownTool;

/// What the server offers. Implemented outside this module, without `rmcp` types.
pub trait Service: Send + Sync + 'static {
    fn tools(&self) -> Vec<ToolSpec>;

    /// Runs one call. `cancel` fires when the client cancels the request or its input ends;
    /// the future is expected to finish soon after.
    fn call(
        self: Arc<Self>,
        name: String,
        arguments: Map<String, Value>,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<ToolOutcome, UnknownTool>> + Send + 'static;
}

/// How serving ended.
pub enum Served {
    /// The client closed the connection.
    Ended,
    /// The connection could not be served; the text says why.
    Failed(String),
}

/// Serves `service` over standard input and output until the client closes the connection.
///
/// rmcp reads and writes two in-memory pipes, which this module copies from standard input and to
/// standard output, so that every failure to read or write is seen here, ends serving, and is
/// reported. Serving returns once every call it started has finished, so that what a call owns,
/// such as a child process, is cleaned up first; all of that is bounded by [`STOP_WITHIN`] from
/// the end of the input.
pub async fn serve<S: Service>(service: S) -> Served {
    let shutdown = CancellationToken::new();
    let fault = Arc::new(Mutex::new(None));
    let (active, mut finished) = mpsc::channel::<()>(1);
    let handler = Handler {
        service: Arc::new(service),
        shutdown: shutdown.clone(),
        active,
    };
    let (server_input, mut input) = tokio::io::duplex(PIPE_BYTES);
    let (server_output, mut output) = tokio::io::duplex(PIPE_BYTES);

    let (ended, input_fault) = (shutdown.clone(), Arc::clone(&fault));
    tokio::spawn(async move {
        let copied = tokio::io::copy(&mut tokio::io::stdin(), &mut input).await;
        record(&input_fault, failed("reading the input", copied));
        // Dropping `input` here is the end of input for rmcp.
        ended.cancel();
    });
    let (stopped, output_fault) = (shutdown.clone(), Arc::clone(&fault));
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let copied = tokio::io::copy(&mut output, &mut stdout).await;
        record(&output_fault, failed("writing the output", copied));
        stopped.cancel();
    });

    let serving = async {
        let failure = match handler.serve((server_input, server_output)).await {
            Ok(running) => quit_failure(running.waiting().await),
            // The input ended before a lifecycle was chosen: nothing was asked.
            Err(ServerInitializeError::ConnectionClosed(_)) => None,
            Err(error) => Some(error.to_string()),
        };
        // rmcp stops draining after five seconds or at the first failed write. Whatever is still
        // running is told to stop and waited for: each call holds a sender until it finishes, and
        // the handler that owned the last one is gone by now. So is rmcp's end of the output
        // pipe, so the writer finishes once it has written what is left.
        shutdown.cancel();
        let _ = finished.recv().await;
        let _ = writer.await;
        failure
    };
    let deadline = async {
        shutdown.cancelled().await;
        tokio::time::sleep(STOP_WITHIN).await;
    };
    let failure = tokio::select! {
        failure = serving => failure,
        () = deadline => Some(format!(
            "running calls and pending output did not finish within {} s of the end of the input",
            STOP_WITHIN.as_secs()
        )),
    };
    let fault = lock(&fault).take();
    fault.or(failure).map_or(Served::Ended, Served::Failed)
}

/// Says what failed, if anything did.
fn failed(what: &str, copied: std::io::Result<u64>) -> Option<String> {
    let error = copied.err()?;
    Some(format!("{what} failed: {error}"))
}

/// Keeps the first failure recorded.
fn record(fault: &Mutex<Option<String>>, failure: Option<String>) {
    let mut slot = lock(fault);
    *slot = slot.take().or(failure);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// rmcp's service loop ended; anything but a closed or cancelled connection is a failure.
fn quit_failure(quit: Result<QuitReason, JoinError>) -> Option<String> {
    let clean = matches!(quit, Ok(QuitReason::Closed | QuitReason::Cancelled));
    let description = format!("the MCP service stopped unexpectedly: {quit:?}");
    (!clean).then_some(description)
}

struct Handler<S> {
    service: Arc<S>,
    shutdown: CancellationToken,
    /// Cloned into every running call; [`serve`] waits until no clone is left.
    active: mpsc::Sender<()>,
}

impl<S: Service> ServerHandler for Handler<S> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }

    /// Every revision rmcp knows except 2025-03-26, which requires a server to accept JSON-RPC
    /// batches; rmcp refuses them. A client that asks for it is offered the newest instead.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[
            ProtocolVersion::V_2024_11_05,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2026_07_28,
        ])
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .service
            .tools()
            .into_iter()
            .map(|spec| Tool::new(spec.name, spec.description, Arc::new(spec.input_schema)))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let cancel = self.shutdown.child_token();
        // If rmcp drops this request, the call is told to stop rather than left running.
        let _cancel_on_drop = cancel.clone().drop_guard();
        let service = Arc::clone(&self.service);
        let arguments = request.arguments.unwrap_or_default();
        let call_name = name.clone();
        let call_cancel = cancel.clone();
        let running = self.active.clone();
        // The call runs as its own task, and is created inside it, so that a panic anywhere in
        // it ends that task, not the server, and can be answered.
        let mut task = tokio::spawn(async move {
            let _running = running;
            service.call(call_name, arguments, call_cancel).await
        });
        let joined = tokio::select! {
            joined = &mut task => joined,
            () = context.ct.cancelled() => {
                cancel.cancel();
                task.await
            }
        };
        match joined {
            Ok(Ok(outcome)) => {
                let content = vec![ContentBlock::text(outcome.text)];
                let result = if outcome.is_error {
                    CallToolResult::error(content)
                } else {
                    CallToolResult::success(content)
                };
                Ok(result.into())
            }
            Ok(Err(UnknownTool)) => Err(ErrorData::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            )),
            Err(_) => Err(ErrorData::internal_error(
                format!("The {name} tool failed unexpectedly; the server is still running."),
                None,
            )),
        }
    }
}
