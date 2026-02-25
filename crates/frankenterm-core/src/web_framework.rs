//! Shared `fastapi` alias surface for the web server module.
//!
//! Keeps framework dependency boundaries explicit and centralized.

pub(crate) use fastapi::core::{BoxFuture, ControlFlow, Cx, Handler, Middleware, StartupOutcome};
pub(crate) use fastapi::http::QueryString;
pub(crate) use fastapi::prelude::{App, Method, Request, RequestContext, Response, StatusCode};
pub(crate) use fastapi::{ResponseBody, ServerConfig, ServerError, TcpServer};
