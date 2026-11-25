//! The MCP server, and the only module that knows `rmcp` (D4).
//!
//! No `rmcp` type leaves this file. The rest of Proxenos implements [`Service`], and this module
//! adds what the library does not do itself: it stops running calls the moment standard input
//! ends, reports a failure to read or write as a failure, hands every call a cancellation token,
//! and answers a call whose handler panicked.

use std::borrow::Cow;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// How long the server may take, from the end of its input, to finish every running call and
/// write what is left of its output. Past it, serving has failed.
const STOP_WITHIN: Duration = Duration::from_secs(10);

/// The size of the in-memory pipes between rmcp and the real standard input and output.
const PIPE_BYTES: usize = 64 * 1024;

/// How much output may wait for a client that does not read it. Past it, serving has failed.
const UNREAD_OUTPUT_LIMIT: usize = 64 << 20;

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
/// rmcp reads and writes two in-memory pipes, and this module moves the bytes between them and the
/// real standard input and output. Standard input is read as soon as it arrives, so its end is seen
/// at once. rmcp's answers are taken from it frame by frame as soon as it writes them, so it never
/// waits on a client that does not read; they reach standard output in order. A failure to read or
/// write is recorded, stops everything, and ends serving with that failure.
///
/// Serving returns once every call it started has finished, so that what a call owns, such as a
/// child process, is cleaned up first. All of that is bounded by [`STOP_WITHIN`] from the end of the
/// input or the first failure.
pub async fn serve<S: Service>(service: S) -> Served {
    // The end of the input, or a failure: calls are told to stop, and the deadline runs.
    let shutdown = CancellationToken::new();
    // A failure: nothing more reaches rmcp.
    let halt = CancellationToken::new();
    let fault = Arc::new(Mutex::new(None));
    let (active, mut finished) = mpsc::channel::<()>(1);
    let handler = Handler {
        service: Arc::new(service),
        shutdown: shutdown.clone(),
        active,
    };
    let (server_input, input) = tokio::io::duplex(PIPE_BYTES);
    let (server_output, output) = tokio::io::duplex(PIPE_BYTES);
    let stopper = Stopper {
        fault: Arc::clone(&fault),
        shutdown: shutdown.clone(),
        halt: halt.clone(),
    };

    let (requests, pending_requests) = mpsc::unbounded_channel();
    tokio::spawn(read_input(requests, stopper.clone()));
    tokio::spawn(forward_input(pending_requests, input, halt.clone()));
    let (answers, pending_answers) = mpsc::unbounded_channel();
    let unread = Arc::new(AtomicUsize::new(0));
    let collector = tokio::spawn(collect_output(
        output,
        answers,
        Arc::clone(&unread),
        stopper.clone(),
    ));
    let writer = tokio::spawn(write_output(pending_answers, unread, stopper));

    let serving = async {
        let failure = match handler.serve((server_input, server_output)).await {
            Ok(running) => quit_failure(running.waiting().await),
            // The input ended before a lifecycle was chosen: nothing was asked.
            Err(ServerInitializeError::ConnectionClosed(_)) => None,
            Err(error) => Some(error.to_string()),
        };
        // rmcp stops draining after five seconds. Whatever is still running is told to stop and
        // waited for: each call holds a sender until it finishes, and the handler that owned the
        // last one is gone by now. So is rmcp's end of the output pipe, so the collector and then
        // the writer finish once what is left has been written.
        shutdown.cancel();
        let _ = finished.recv().await;
        let _ = collector.await;
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

/// Ends serving: with a failure, everything stops; without one, the input has ended.
#[derive(Clone)]
struct Stopper {
    fault: Arc<Mutex<Option<String>>>,
    shutdown: CancellationToken,
    halt: CancellationToken,
}

impl Stopper {
    fn stop(&self, failure: Option<String>) {
        if failure.is_some() {
            self.halt.cancel();
        }
        record(&self.fault, failure);
        self.shutdown.cancel();
    }
}

/// Reads standard input to its end, whatever rmcp is doing.
async fn read_input(requests: mpsc::UnboundedSender<Vec<u8>>, stopper: Stopper) {
    let mut stdin = tokio::io::stdin();
    let mut chunk = vec![0_u8; PIPE_BYTES];
    let failure = loop {
        let read = stdin.read(&mut chunk).await;
        let Ok(count @ 1..) = read else {
            break failed("reading the input", read);
        };
        // Once forwarding has stopped there is nobody to give the bytes to.
        let _ = requests.send(chunk[..count].to_vec());
    };
    stopper.stop(failure);
}

/// Gives rmcp what was read, in order, until the input ends or serving fails. Dropping `input` is
/// the end of input for rmcp.
async fn forward_input(
    mut requests: mpsc::UnboundedReceiver<Vec<u8>>,
    mut input: DuplexStream,
    halt: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            next = requests.recv() => next,
            () = halt.cancelled() => None,
        };
        let Some(bytes) = next else { break };
        // Once rmcp has stopped serving it has dropped its end, and the bytes have nowhere to go.
        let _ = input.write_all(&bytes).await;
    }
}

/// Takes rmcp's answers frame by frame as it writes them.
async fn collect_output(
    output: DuplexStream,
    answers: mpsc::UnboundedSender<Vec<u8>>,
    unread: Arc<AtomicUsize>,
    stopper: Stopper,
) {
    let mut output = BufReader::new(output);
    let failure = loop {
        let mut frame = Vec::new();
        let Ok(1..) = output.read_until(b'\n', &mut frame).await else {
            break None;
        };
        let waiting = unread.fetch_add(frame.len(), Ordering::SeqCst) + frame.len();
        let _ = answers.send(frame);
        if waiting > UNREAD_OUTPUT_LIMIT {
            break Some(format!(
                "more than {} MiB of output was waiting for the client to read it",
                UNREAD_OUTPUT_LIMIT >> 20
            ));
        }
    };
    // Without a failure, rmcp has ended, and so has serving.
    stopper.stop(failure);
}

/// Writes the answers to standard output, each in one piece.
async fn write_output(
    mut answers: mpsc::UnboundedReceiver<Vec<u8>>,
    unread: Arc<AtomicUsize>,
    stopper: Stopper,
) {
    let mut stdout = tokio::io::stdout();
    let failure = loop {
        let Some(frame) = answers.recv().await else {
            break None;
        };
        let written = write_frame(&mut stdout, &frame).await;
        unread.fetch_sub(frame.len(), Ordering::SeqCst);
        let failure = failed("writing the output", written);
        if failure.is_some() {
            break failure;
        }
    };
    stopper.stop(failure);
}

async fn write_frame(stdout: &mut tokio::io::Stdout, frame: &[u8]) -> std::io::Result<()> {
    stdout.write_all(frame).await?;
    stdout.flush().await
}

/// Says what failed, if anything did.
fn failed<T>(what: &str, result: std::io::Result<T>) -> Option<String> {
    let error = result.err()?;
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
