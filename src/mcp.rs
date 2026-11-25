//! The MCP server, and the only module that knows `rmcp` (D4).
//!
//! No `rmcp` type leaves this file. The rest of Proxenos implements [`Service`], and this module
//! adds what the library does not do itself: it stops waiting the moment standard input ends,
//! hands every call a cancellation token, and answers a call whose handler panicked.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, ServerInitializeError};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

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
pub async fn serve<S: Service>(service: S) -> Served {
    let shutdown = CancellationToken::new();
    let handler = Handler {
        service: Arc::new(service),
        shutdown: shutdown.clone(),
    };
    let (stdin, stdout) = rmcp::transport::stdio();
    let reader = EndSignal {
        inner: stdin,
        ended: shutdown,
    };
    match handler.serve((reader, stdout)).await {
        Ok(running) => served(
            running
                .waiting()
                .await
                .err()
                .as_ref()
                .map(ToString::to_string),
        ),
        // Standard input ended before a lifecycle was chosen: nothing was asked.
        Err(ServerInitializeError::ConnectionClosed(_)) => Served::Ended,
        Err(error) => Served::Failed(error.to_string()),
    }
}

fn served(failure: Option<String>) -> Served {
    failure.map_or(Served::Ended, Served::Failed)
}

struct Handler<S> {
    service: Arc<S>,
    shutdown: CancellationToken,
}

impl<S: Service> ServerHandler for Handler<S> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
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
        // The handler runs as its own task so that a panic in it ends that task, not the
        // server, and can be answered.
        let mut task = tokio::spawn(Arc::clone(&self.service).call(
            name.clone(),
            request.arguments.unwrap_or_default(),
            cancel.clone(),
        ));
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

/// Passes reads through and cancels `ended` when standard input ends or fails.
///
/// `rmcp` itself waits five seconds after its input closes before it cancels running calls.
struct EndSignal<R> {
    inner: R,
    ended: CancellationToken,
}

impl<R: AsyncRead + Unpin> AsyncRead for EndSignal<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let ended = matches!(poll, Poll::Ready(Err(_)))
            || (matches!(poll, Poll::Ready(Ok(())))
                && buf.filled().len() == before
                && buf.remaining() > 0);
        if ended {
            self.ended.cancel();
        }
        poll
    }
}
