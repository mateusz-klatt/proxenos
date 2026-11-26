//! Codex app-server serves account/rateLimits/read after its own stdio initialization.
use std::future::Future;
use std::io;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, Take};
use tokio::process::ChildStdout;
use tokio_util::sync::CancellationToken;

use crate::discovery::Executable;
use crate::mcp::Activity;
use crate::platform;
use crate::supervise::{Ending, StartError, check_start, write_stdin};

type Output = Take<BufReader<ChildStdout>>;
type ReadResult<T> = Result<T, &'static str>;

async fn response(reader: &mut Output, id: i64) -> ReadResult<Value> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let size = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| "unavailable")?;
        if reader.limit() == 0 {
            return Err("capture");
        }
        if size == 0 {
            return Err("invalid_report");
        }
        let mut frame: Value = serde_json::from_slice(&line).map_err(|_| "invalid_report")?;
        if frame["id"].as_i64() != Some(id) || frame.get("method").is_some() {
            continue;
        }
        match (frame.get("result"), frame.get("error")) {
            (Some(result), None) if result.is_object() => return Ok(frame["result"].take()),
            (None, Some(error)) if error.is_object() => {
                let login = id == 2
                    && error["code"].as_i64() == Some(-32600)
                    && error["message"]
                        == "codex account authentication required to read rate limits";
                return Err(if login {
                    "authentication"
                } else {
                    "unavailable"
                });
            }
            _ => return Err("invalid_report"),
        }
    }
}

async fn drain<R: AsyncRead + Unpin>(reader: &mut Take<R>) -> ReadResult<()> {
    tokio::io::copy(reader, &mut tokio::io::sink())
        .await
        .map_err(|_| "unavailable")?;
    (reader.limit() != 0).then_some(()).ok_or("capture")
}

async fn exchange(mut reader: Output, writer: io::PipeWriter) -> ReadResult<Value> {
    let mut writer = Some(writer);
    let init = json!({"id":1, "method":"initialize", "params":{"clientInfo":{
        "name":"proxenos", "version":env!("CARGO_PKG_VERSION")
    }}});
    write_stdin(&mut writer, format!("{init}\n"))
        .await
        .map_err(|_| "unavailable")?;
    response(&mut reader, 1).await?;
    let ready = json!({"method":"initialized"});
    let limits = json!({"id":2, "method":"account/rateLimits/read", "params":{"excludeResetCreditDetails":true}});
    write_stdin(&mut writer, format!("{ready}\n{limits}\n"))
        .await
        .map_err(|_| "unavailable")?;
    let result = response(&mut reader, 2).await?;
    drop(writer);
    drain(&mut reader).await?;
    Ok(result)
}

async fn bounded<T>(
    deadline: Instant,
    cancel: &CancellationToken,
    operation: impl Future<Output = ReadResult<T>>,
) -> ReadResult<T> {
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        () = tokio::time::sleep_until(deadline.into()) => None,
        result = operation => Some(result),
    };
    // A native spawn can observe expiry before Tokio delivers the timer tick.
    if cancel.is_cancelled() {
        return Err("cancelled");
    }
    result
        .filter(|_| Instant::now() < deadline)
        .unwrap_or(Err("timeout"))
}

pub(super) async fn read(
    exe: Executable,
    deadline: Instant,
    cancel: &CancellationToken,
    activity: Activity,
) -> ReadResult<Value> {
    let mut command = Command::new(exe.program);
    command
        .args(exe.prefix)
        .args(["app-server", "--listen", "stdio://"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // The worker rechecks a dropped request under the native spawn lock. A handle that
    // arrives after its receiver is gone still owns the group and will stop it on drop.
    let queued = cancel.child_token();
    let _abandoned = queued.clone().drop_guard();
    let launched = tokio::task::spawn_blocking(move || {
        platform::spawn(
            command,
            platform::Input::Piped,
            &mut Vec::new(),
            None,
            activity,
            || check_start(&queued, deadline),
        )
    });
    let mut child = bounded(deadline, cancel, async {
        launched
            .await
            .expect("usage spawn worker finished")
            .map_err(|reason| match reason {
                StartError::Skipped(Ending::Cancelled) => "cancelled",
                StartError::Skipped(_) => "timeout",
                StartError::Io(_) => "unavailable",
            })
    })
    .await?;
    let writer = child.stdin.take().expect("stdin was piped");
    let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"))
        .take(super::REPORT_LIMIT as u64 + 1);
    let mut stderr = child
        .stderr
        .take()
        .expect("stderr was piped")
        .take(super::REPORT_LIMIT as u64 + 1);
    let result = bounded(deadline, cancel, async {
        let mut pipes = pin!(async {
            let (value, ()) = tokio::try_join!(exchange(stdout, writer), drain(&mut stderr))?;
            Ok(value)
        });
        let mut wait = pin!(child.wait_for_exit());
        tokio::select! {
            answer = &mut pipes => {
                let answer = answer?;
                wait.await.map_err(|_| "unavailable")?;
                Ok(answer)
            }
            status = &mut wait => {
                status.map_err(|_| "unavailable")?;
                // A descendant may hold the pipes open even after the app-server exits.
                tokio::time::timeout(Duration::from_secs(2), pipes).await.map_err(|_| "timeout")?
            }
        }
    })
    .await;
    if result.is_err() {
        let _ = child.stop().await;
    }
    let (status, _) = child.finish().await;
    let report = result?;
    status
        .is_ok_and(|s| s.success())
        .then_some(report)
        .ok_or("unavailable")
}
