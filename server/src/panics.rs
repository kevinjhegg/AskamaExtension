//! Keeping the server alive through a panic in a request or notification.
//!
//! tower-lsp runs every handler inside the future of the server's main loop, so a panic in any of them would unwind out of `main` and end
//! the process, losing the open documents, the Rust index and the private rust-analyzers. [`CatchPanics`] wraps the whole service instead:
//! a request that panics is answered with an internal error, so the editor does not wait for an answer that never comes, the panic is
//! reported, and the server goes on. Tasks the server spawns, such as type checks, are already apart: a panic ends only that task.
//!
//! What a handler changed in the shared state before it panicked stays as it was. The handlers change it in small steps, each one whole (an
//! insert or a remove), and the state's lock ignores poisoning, so the server goes on with state that is still consistent.

use std::any::Any;
use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::FutureExt;
use futures::future::BoxFuture;
use tower_lsp_server::jsonrpc::{Error, Request, Response};
use tower_service::Service;

/// What is told about a panic: a sentence naming the method and giving the panic's message.
pub type Report = Arc<dyn Fn(String) + Send + Sync>;

/// A language server service whose requests and notifications cannot end the process by panicking.
pub struct CatchPanics<S> {
    /// The service that does the work.
    inner: S,
    /// Where each panic is reported.
    report: Report,
}

impl<S> CatchPanics<S> {
    /// Wraps `inner`, reporting each panic to `report`.
    pub fn new(inner: S, report: Report) -> Self {
        Self { inner, report }
    }
}

impl<S> Service<Request> for CatchPanics<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Option<Response>, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let method = request.method().to_owned();
        let id = request.id().cloned();
        let report = Arc::clone(&self.report);
        // A handler runs when its future is polled, but routing the request (reading its parameters) runs here, so both are caught.
        let future = catch_unwind(AssertUnwindSafe(|| self.inner.call(request)));
        Box::pin(async move {
            let outcome = match future {
                Ok(future) => AssertUnwindSafe(future).catch_unwind().await,
                Err(panic) => Err(panic),
            };
            outcome.unwrap_or_else(|panic| {
                let message = format!("askama-lsp: `{method}` panicked: {}. Please report this.", panic_message(&*panic));
                report(message.clone());
                // A notification has no answer; a request gets an internal error.
                Ok(id.map(|id| Response::from_error(id, Error { message: Cow::Owned(message), ..Error::internal_error() })))
            })
        })
    }
}

/// The message a panic was raised with, which is a `&str` or a `String` for every `panic!`.
fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Mutex;

    use futures::future::{self, Ready};
    use serde_json::json;
    use tower_lsp_server::jsonrpc::Id;

    use super::*;

    /// A service that panics on `panic` (when its future is polled) and on `route` (when it is called), and answers everything else with
    /// `null`.
    struct Fragile;

    impl Service<Request> for Fragile {
        type Response = Option<Response>;
        type Error = Infallible;
        type Future = futures::future::Either<Ready<Result<Option<Response>, Infallible>>, BoxFuture<'static, Result<Option<Response>, Infallible>>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: Request) -> Self::Future {
            match request.method() {
                "route" => panic!("cannot route"),
                "panic" => futures::future::Either::Right(Box::pin(async { panic!("handler failed: {}", 42) })),
                _ => futures::future::Either::Left(future::ready(Ok(request.id().cloned().map(|id| Response::from_ok(id, json!(null)))))),
            }
        }
    }

    /// `Fragile` wrapped, with the reports it makes.
    fn wrapped() -> (CatchPanics<Fragile>, Arc<Mutex<Vec<String>>>) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&reports);
        (CatchPanics::new(Fragile, Arc::new(move |message| kept.lock().unwrap().push(message))), reports)
    }

    #[tokio::test]
    async fn answers_a_request_that_panics_with_an_internal_error() {
        let (mut service, reports) = wrapped();
        let response = service.call(Request::build("panic").id(Id::Number(7)).finish()).await.unwrap().unwrap();
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32603);
        assert!(response["error"]["message"].as_str().unwrap().contains("`panic` panicked: handler failed: 42"));
        assert_eq!(reports.lock().unwrap().len(), 1);

        // The service goes on answering.
        let response = service.call(Request::build("fine").id(Id::Number(8)).finish()).await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(response).unwrap()["result"], json!(null));
    }

    #[tokio::test]
    async fn catches_a_panic_while_routing_and_gives_a_notification_no_answer() {
        let (mut service, reports) = wrapped();
        assert!(service.call(Request::build("route").finish()).await.unwrap().is_none());
        assert!(service.call(Request::build("panic").finish()).await.unwrap().is_none());
        let reports = reports.lock().unwrap();
        assert!(reports[0].contains("`route` panicked: cannot route"), "{reports:?}");
        assert_eq!(reports.len(), 2);
    }
}
