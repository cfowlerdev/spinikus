//! On-demand sessions. The control plane asks for one with an `OpenSession` command on the
//! control stream; the agent opens an `AgentService.OpenSession` stream whose first frame
//! carries the session ID, and serves it until either side closes it. A log tail sends the
//! container's log as `log_line` frames, then `closed`; a terminal runs a command with a TTY,
//! relaying `data` and `resize` frames both ways, then `closed` with its exit code.

use std::{sync::Arc, time::Duration};

use spinikus_proto::agent::v1::{self as pb, agent_service_client::AgentServiceClient, open_session::Kind, session_frame::Frame};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{Streaming, transport::Channel};

use crate::runtime::{ContainerRuntime, LogOptions};

mod terminal;

/// How long a finished session waits for the control plane to close its side.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// Opens the session in the background.
pub(crate) fn open(client: AgentServiceClient<Channel>, runtime: Arc<dyn ContainerRuntime>, open: pb::OpenSession) {
    tokio::spawn(async move {
        let id = open.session_id.clone();
        if let Err(status) = serve(client, &*runtime, open).await {
            tracing::warn!(session = id, %status, "failed to open a session");
        }
    });
}

fn closed(error: impl Into<String>) -> pb::SessionFrame {
    pb::SessionFrame {
        session_id: String::new(),
        frame: Some(Frame::Closed(pb::SessionClosed { error: error.into(), exit_code: None })),
    }
}

async fn serve(
    mut client: AgentServiceClient<Channel>,
    runtime: &dyn ContainerRuntime,
    open: pb::OpenSession,
) -> Result<(), tonic::Status> {
    let (sender, receiver) = mpsc::channel(64);
    let first = pb::SessionFrame { session_id: open.session_id.clone(), frame: None };
    sender.send(first).await.expect("the receiver is held below");
    let mut inbound = client.open_session(ReceiverStream::new(receiver)).await?.into_inner();
    let inbound = match open.kind {
        Some(Kind::LogTail(tail)) => {
            log_tail(runtime, tail, &sender, &mut inbound).await;
            Some(inbound)
        }
        Some(Kind::Terminal(terminal)) => terminal::serve(runtime, terminal, &sender, inbound).await,
        None => {
            let _ = sender.send(closed("this agent cannot open this kind of session")).await;
            Some(inbound)
        }
    };
    // Dropping the response stream resets the call, losing frames not yet sent: end our side
    // and wait for the control plane to close its own.
    drop(sender);
    if let Some(mut inbound) = inbound {
        let drained = async { while let Ok(Some(_)) = inbound.message().await {} };
        let _ = tokio::time::timeout(CLOSE_GRACE, drained).await;
    }
    Ok(())
}

/// Sends the container's log until it ends, or the control plane stops reading.
async fn log_tail(
    runtime: &dyn ContainerRuntime,
    tail: pb::LogTailSession,
    sender: &mpsc::Sender<pb::SessionFrame>,
    inbound: &mut Streaming<pb::SessionFrame>,
) {
    match runtime.inspect(&tail.container_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = sender.send(closed(format!("no container `{}` managed by Spinikus", tail.container_id))).await;
            return;
        }
        Err(error) => {
            let _ = sender.send(closed(error.to_string())).await;
            return;
        }
    }
    let since = tail.since.and_then(|since| jiff::Timestamp::new(since.seconds, since.nanos).ok());
    let options = LogOptions { since, tail: (tail.tail > 0).then_some(tail.tail), follow: tail.follow };
    let mut lines = runtime.logs(&tail.container_id, options);
    loop {
        tokio::select! {
            line = lines.next() => {
                let frame = match line {
                    Some(Ok(line)) => Frame::LogLine(pb::LogLine {
                        at: line.at.map(|at| prost_types::Timestamp { seconds: at.as_second(), nanos: at.subsec_nanosecond() }),
                        stream: match line.stderr {
                            true => pb::LogStream::Stderr,
                            false => pb::LogStream::Stdout,
                        }
                        .into(),
                        text: line.text,
                    }),
                    Some(Err(error)) => Frame::Closed(pb::SessionClosed { error: error.to_string(), exit_code: None }),
                    None => Frame::Closed(pb::SessionClosed::default()),
                };
                let last = matches!(frame, Frame::Closed(_));
                if sender.send(pb::SessionFrame { session_id: String::new(), frame: Some(frame) }).await.is_err() || last {
                    return;
                }
            }
            // The control plane closes its side once nobody reads the log any more.
            message = inbound.message() => if !matches!(message, Ok(Some(_))) {
                return;
            },
        }
    }
}
