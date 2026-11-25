//! The MCP server, and the only module that knows `rmcp` (D4).
//!
//! No `rmcp` type leaves this file. The rest of Proxenos implements [`Service`], and this module
//! adds what the library does not do itself: it stops running calls when it sees standard input
//! end, reports a failure to read or write as a failure, hands every call a cancellation token,
//! and answers a call whose handler panicked.

use std::borrow::Cow;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
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

/// How many input chunks can wait for rmcp before the reader stops reading ahead.
const INPUT_CHUNKS: usize = 16;

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

/// A call and its background workers keep serving alive until their last handle drops.
#[derive(Clone)]
pub struct Activity {
    _running: mpsc::Sender<()>,
    shutdown: CancellationToken,
}

impl Activity {
    /// A child token that fires only when serving shuts down, never when a single request ends.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.child_token()
    }
}

impl Activity {
    pub fn track() -> (Self, mpsc::Receiver<()>) {
        let (running, finished) = mpsc::channel(1);
        (
            Self {
                _running: running,
                shutdown: CancellationToken::new(),
            },
            finished,
        )
    }
}

/// What the server offers. Implemented outside this module, without `rmcp` types.
pub trait Service: Send + Sync + 'static {
    fn tools(
        &self,
        cancel: &CancellationToken,
        activity: Activity,
    ) -> impl Future<Output = Vec<ToolSpec>> + Send;

    /// Runs one call. `cancel` fires when the client cancels the request or its input ends;
    /// the future is expected to finish soon after.
    fn call(
        self: Arc<Self>,
        name: String,
        arguments: Map<String, Value>,
        cancel: CancellationToken,
        activity: Activity,
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
/// real standard input and output. The reader sees the end of input late only while rmcp lags by
/// more than the bounded input queue can hold. Answers reach standard output in frame order.
/// A failure to read or write stops serving and determines the exit status.
///
/// Serving returns once every call it started has finished, so that what a call owns, such as a
/// child process, is cleaned up first. All of that is bounded by [`STOP_WITHIN`] from the end of the
/// input or the first failure.
pub async fn serve<S: Service>(service: S) -> Served {
    let stopper = Arc::new(Stopper::default());
    let (active, mut finished) = mpsc::channel(1);
    let handler = Handler {
        service: Arc::new(service),
        stopper: Arc::clone(&stopper),
        active: Activity {
            _running: active,
            shutdown: stopper.shutdown.clone(),
        },
    };
    let (server_input, input) = tokio::io::duplex(PIPE_BYTES);
    let (server_output, output) = tokio::io::duplex(PIPE_BYTES);
    let (requests, pending_requests) = mpsc::channel(INPUT_CHUNKS);
    tokio::spawn(read_input(requests, Arc::clone(&stopper)));
    tokio::spawn(forward_input(pending_requests, input, Arc::clone(&stopper)));
    let (answers, pending_answers) = mpsc::unbounded_channel();
    let unread = Arc::new(AtomicUsize::new(0));
    let collector = tokio::spawn(collect_output(
        output,
        answers,
        Arc::clone(&unread),
        Arc::clone(&stopper),
    ));
    let writer = tokio::spawn(write_output(pending_answers, unread, Arc::clone(&stopper)));

    let serving = async {
        let failure = match handler.serve((server_input, server_output)).await {
            Ok(running) => quit_failure(running.waiting().await),
            // The input ended before a lifecycle was chosen: nothing was asked.
            Err(ServerInitializeError::ConnectionClosed(_)) => None,
            Err(error) => Some(error.to_string()),
        };
        // Kept before the cleanup below, which the deadline may cut short. Output still drains:
        // a client that reads may be owed the answer that explains the failure.
        stopper.record(failure);
        // rmcp stops draining after five seconds. Whatever is still running is told to stop and
        // waited for. Each call and any worker still reaping its child hold a sender. The handler
        // has dropped its copy, as well as rmcp's end of the output pipe, so the collector and then
        // the writer finish once what is left has been written. On failure stdout may be blocked.
        stopper.shutdown.cancel();
        let _ = finished.recv().await;
        tokio::select! {
            biased;
            () = stopper.halt.cancelled() => (),
            () = async {
                let _ = collector.await;
                let _ = writer.await;
            } => (),
        }
    };
    let deadline = async {
        stopper.shutdown.cancelled().await;
        tokio::time::sleep(STOP_WITHIN).await;
    };
    tokio::select! {
        () = serving => (),
        () = deadline => stopper.record(Some(format!(
            "running calls and pending output did not finish within {} s of the end of serving",
            STOP_WITHIN.as_secs()
        ))),
    }
    let fault = lock(&stopper.fault).take();
    fault.map_or(Served::Ended, Served::Failed)
}

/// Ends serving: with a failure, everything stops; without one, the input has ended.
#[derive(Default)]
struct Stopper {
    fault: Mutex<Option<String>>,
    shutdown: CancellationToken,
    halt: CancellationToken,
}

impl Stopper {
    fn stop(&self, failure: Option<String>) {
        if let Some(reason) = failure {
            let mut fault = lock(&self.fault);
            fault.get_or_insert(reason);
            self.halt.cancel();
        }
        self.shutdown.cancel();
    }

    /// Keeps the first failure without halting anything.
    fn record(&self, failure: Option<String>) {
        let mut fault = lock(&self.fault);
        *fault = fault.take().or(failure);
    }

    /// Orders a synchronous operation against halt on another worker. No lock spans an await.
    fn unless_halted<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        let _guard = lock(&self.fault);
        (!self.halt.is_cancelled()).then(operation)
    }
}

/// Reads ahead until a full queue puts backpressure on the client.
async fn read_input(requests: mpsc::Sender<Vec<u8>>, stopper: Arc<Stopper>) {
    let mut stdin = tokio::io::stdin();
    let mut chunk = vec![0_u8; PIPE_BYTES];
    while !requests.is_closed() {
        let read = stdin.read(&mut chunk).await;
        let Ok(count @ 1..) = read else {
            stopper.stop(failed("reading the input", read));
            break;
        };
        let _ = requests.send(chunk[..count].to_vec()).await;
    }
}

/// Gives rmcp what was read, in order, until the input ends or serving fails. Dropping `input` is
/// the end of input for rmcp.
async fn forward_input(
    mut requests: mpsc::Receiver<Vec<u8>>,
    mut input: DuplexStream,
    stopper: Arc<Stopper>,
) {
    loop {
        let mut step = pin!(async {
            let bytes = requests.recv().await?;
            input.write_all(&bytes).await.ok()
        });
        // The token wakes a blocked write. The lock also covers a write that becomes ready
        // between polling the token and polling the pipe on this worker.
        let sent = tokio::select! {
            biased;
            () = stopper.halt.cancelled() => None,
            sent = poll_fn(|cx| {
                stopper.unless_halted(|| step.as_mut().poll(cx)).unwrap_or(Poll::Ready(None))
            }) => sent,
        };
        if sent.is_none() {
            break;
        }
    }
}

/// Counts unread bytes as they arrive and gives the writer only complete frames.
async fn collect_output(
    output: DuplexStream,
    answers: mpsc::UnboundedSender<Vec<u8>>,
    unread: Arc<AtomicUsize>,
    stopper: Arc<Stopper>,
) {
    let mut output = BufReader::new(output);
    let mut frame = Vec::new();
    let failure = loop {
        let read = output.fill_buf().await;
        let Ok(bytes @ [_, ..]) = read else {
            let truncated = (!frame.is_empty()).then_some("MCP output ended during a frame".into());
            break failed("reading MCP output", read).or(truncated);
        };
        let count = bytes
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        let waiting = unread.fetch_add(count, Ordering::SeqCst) + count;
        if waiting > UNREAD_OUTPUT_LIMIT {
            break Some(format!(
                "more than {} MiB of output was waiting for the client to read it",
                UNREAD_OUTPUT_LIMIT >> 20
            ));
        }
        frame.extend_from_slice(&bytes[..count]);
        output.consume(count);
        if frame.ends_with(b"\n") {
            let _ = answers.send(std::mem::take(&mut frame));
        }
    };
    // Without a failure, rmcp has ended, and so has serving.
    stopper.stop(failure);
}

/// Writes the answers to standard output, each in one piece.
async fn write_output(
    mut answers: mpsc::UnboundedReceiver<Vec<u8>>,
    unread: Arc<AtomicUsize>,
    stopper: Arc<Stopper>,
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
    stopper: Arc<Stopper>,
    /// Cloned into every running call; [`serve`] waits until no clone is left.
    active: Activity,
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
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let cancel = self.stopper.shutdown.child_token();
        let stop_on_drop = cancel.clone().drop_guard();
        let listing = self.service.tools(&cancel, self.active.clone());
        tokio::pin!(listing);
        let tools = tokio::select! {
            tools = &mut listing => tools,
            () = context.ct.cancelled() => {
                cancel.cancel();
                listing.await
            }
        }
        .into_iter()
        .map(|spec| Tool::new(spec.name, spec.description, Arc::new(spec.input_schema)))
        .collect();
        stop_on_drop.disarm();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name;
        let stopper = Arc::clone(&self.stopper);
        let cancel = stopper.shutdown.child_token();
        // If rmcp drops this request, the call is told to stop rather than left running.
        let cancel_on_drop = cancel.clone().drop_guard();
        let service = Arc::clone(&self.service);
        let arguments = request.arguments.unwrap_or_default();
        let call_name = name.clone();
        let call_cancel = cancel.clone();
        let running = self.active.clone();
        // The call runs as its own task, and is created inside it, so that a panic anywhere in
        // it ends that task, not the server, and can be answered.
        let mut task = tokio::spawn(async move {
            // The tool's work runs when its future is polled, so the lock covers only the check.
            stopper
                .unless_halted(|| ())
                .ok_or(ErrorData::internal_error(
                    "The MCP connection has failed.",
                    None,
                ))?;
            // The task owns its handle through the whole call, even if the service transfers
            // its copy to a worker that finishes before the call constructs its result.
            let call = service.call(
                call_name.to_string(),
                arguments,
                call_cancel,
                running.clone(),
            );
            call.await.map_err(|UnknownTool| {
                ErrorData::invalid_params(format!("Unknown tool: {call_name}"), None)
            })
        });
        let joined = tokio::select! {
            joined = &mut task => joined,
            () = context.ct.cancelled() => {
                cancel.cancel();
                task.await
            }
        };
        cancel_on_drop.disarm();
        let outcome = joined.map_err(|_| {
            ErrorData::internal_error(
                format!("The {name} tool failed unexpectedly; the server is still running."),
                None,
            )
        })??;
        let content = vec![ContentBlock::text(outcome.text)];
        let result = if outcome.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        Ok(result.into())
    }
}
