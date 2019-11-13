//! A simple HTTP server, for learning and local doc development.

#[macro_use]
extern crate derive_more;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;

use env_logger::{Builder, Env};
use handlebars::Handlebars;
use http::status::StatusCode;
use http::Uri;
use hyper::service::{make_service_fn, service_fn};
use hyper::{header, Body, Request, Response, Server};
use percent_encoding::percent_decode_str;
use std::error::Error as StdError;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use tokio::runtime::Runtime;

// Developer extensions
mod ext;

fn main() {
    // Set up our error handling immediately
    if let Err(e) = run() {
        log_error_chain(&e);
    }
}

/// Basic error reporting, including the "cause chain". This is used both by the
/// top-level error reporting and to report internal server errors.
fn log_error_chain(mut e: &dyn StdError) {
    error!("error: {}", e);
    while let Some(source) = e.source() {
        error!("caused by: {}", source);
        e = source;
    }
}

fn run() -> Result<()> {
    // Initialize logging, and log the "info" level for this crate only, unless
    // the environment contains `RUST_LOG`.
    let env = Env::new().default_filter_or("basic_http_server=info");
    Builder::from_env(env)
        .default_format_module_path(false)
        .default_format_timestamp(false)
        .init();

    // Create the configuration from the command line arguments. It
    // includes the IP address and port to listen on and the path to use
    // as the HTTP server's root directory.
    let config = Config::from_args();

    // Display the configuration to be helpful
    info!("basic-http-server {}", env!("CARGO_PKG_VERSION"));
    info!("addr: http://{}", config.addr);
    info!("root dir: {}", config.root_dir.display());
    info!("extensions: {}", config.use_extensions);

    // Create the service builder that creates a new Hyper service for every
    // connection
    let make_service = make_service_fn(|_| {
        let config = config.clone();

        async {
            let service = service_fn(move |req| {
                let config = config.clone();

                async {
                    let resp = serve(config, req).await;

                    Ok::<_, Error>(resp)
                }
            });

            Ok::<_, Error>(service)
        }
    });

    let server = Server::bind(&config.addr).serve(make_service);

    let rt = Runtime::new()?;
    rt.block_on(server)?;

    Ok(())
}

/// The configuration object, parsed from command line options
#[derive(Clone, StructOpt)]
#[structopt(about = "A basic HTTP file server")]
pub struct Config {
    /// Sets the IP:PORT combination
    #[structopt(
        name = "ADDR",
        short = "a",
        long = "addr",
        parse(try_from_str),
        default_value = "127.0.0.1:4000"
    )]
    addr: SocketAddr,
    /// Sets the root dir
    #[structopt(name = "ROOT", parse(from_os_str), default_value = ".")]
    root_dir: PathBuf,
    /// Enable developer extensions
    #[structopt(short = "x")]
    use_extensions: bool,
}

/// The function that returns a future of an HTTP response for each hyper
/// Request that is received. Errors are turned into an Error response (404 or
/// 500), and never propagated upward for hyper to deal with.
async fn serve(config: Config, req: Request<Body>) -> Response<Body> {
    let resp = serve_file(&req, &config.root_dir).await;

    // Give developer extensions an opportunity to post-process the request/response pair
    let resp = ext::serve(config, req, resp).await;

    // Transform internal errors to error responses
    let resp = transform_error(resp).await;

    resp
}

/// Turn any errors into an HTTP error response.
async fn transform_error(resp: Result<Response<Body>>) -> Response<Body> {
    match resp {
        Ok(r) => r,
        Err(e) => {
            let resp = make_error_response(e).await;
            match resp {
                Ok(r) => r,
                Err(e) => {
                    // Last-ditch error reporting
                    error!("unexpected internal error: {}", e);
                    Response::new(Body::from(format!("unexpected internal error: {}", e)))
                }
            }
        }
    }
}

/// Serve static files from a root directory
async fn serve_file(req: &Request<Body>, root_dir: &PathBuf) -> Result<Response<Body>> {
    // First, try to do a redirect per `try_dir_redirect`. If that doesn't
    // happen, then find the path to the static file we want to serve - which
    // may be `index.html` for directories - and send a response containing that
    // file.
    let maybe_redir_resp = try_dir_redirect(req, &root_dir)?;

    if let Some(redir_resp) = maybe_redir_resp {
        return Ok(redir_resp);
    }

    if let Some(path) = local_path_with_maybe_index(req.uri(), &root_dir) {
        Ok(respond_with_file(path).await?)
    } else {
        Err(Error::UrlToPath)
    }
}

/// If we get a URL without trailing "/" that can be mapped to a directory, then
/// return a 302 redirect to the path with the trailing "/".
///
/// Without this we couldn't correctly return the contents of `index.html` for a
/// directory - for the purpose of building absolute URLs from relative URLs,
/// agents appear to only treat paths with trailing "/" as directories, so we
/// have to redirect to the proper directory URL first.
///
/// In other words, if we returned the contents of `index.html` for URL `docs`
/// then all the relative links in that file would be broken, but that is not
/// the case for URL `docs/`.
///
/// This seems to match the behavior of other static web servers.
fn try_dir_redirect(req: &Request<Body>, root_dir: &PathBuf) -> Result<Option<Response<Body>>> {
    if !req.uri().path().ends_with("/") {
        debug!("path does not end with /");
        if let Some(path) = local_path_for_request(req.uri(), root_dir) {
            if path.is_dir() {
                let mut new_loc = req.uri().path().to_string();
                new_loc.push_str("/");
                if let Some(query) = req.uri().query() {
                    new_loc.push_str("?");
                    new_loc.push_str(query);
                }
                info!("redirecting {} to {}", req.uri(), new_loc);
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, new_loc)
                    .body(Body::empty())
                    .map(Some)
                    .map_err(Error::from)
            } else {
                Ok(None)
            }
        } else {
            Err(Error::UrlToPath)
        }
    } else {
        Ok(None)
    }
}

/// Read the file completely and construct a 200 response with that file as the
/// body of the response. If the I/O here fails then an error future will be
/// returned, and `serve` will convert it into the appropriate HTTP error
/// response.
async fn respond_with_file(path: PathBuf) -> Result<Response<Body>> {
    let buf = tokio::fs::read(&path).await?;
    let mime_type = file_path_mime(&path);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, buf.len() as u64)
        .header(header::CONTENT_TYPE, mime_type.as_ref())
        .body(Body::from(buf))?;
    Ok(resp)
}

/// Get a MIME type based on the file extension
fn file_path_mime(file_path: &Path) -> mime::Mime {
    let mime_type = match file_path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("html") => mime::TEXT_HTML,
        Some("css") => mime::TEXT_CSS,
        Some("js") => mime::TEXT_JAVASCRIPT,
        Some("jpg") => mime::IMAGE_JPEG,
        Some("md") => "text/markdown; charset=UTF-8"
            .parse::<mime::Mime>()
            .unwrap(),
        Some("png") => mime::IMAGE_PNG,
        Some("svg") => mime::IMAGE_SVG,
        Some("wasm") => "application/wasm".parse::<mime::Mime>().unwrap(),
        _ => mime::TEXT_PLAIN,
    };
    mime_type
}

/// Find the local path for a request URI, converting directories to the
/// `index.html` file.
fn local_path_with_maybe_index(uri: &Uri, root_dir: &Path) -> Option<PathBuf> {
    local_path_for_request(uri, root_dir).map(|mut p: PathBuf| {
        if p.is_dir() {
            p.push("index.html");
            debug!("trying {} for directory URL", p.display());
        } else {
            trace!("trying path as from URL");
        }
        p
    })
}

/// Map the request's URI to a local path
fn local_path_for_request(uri: &Uri, root_dir: &Path) -> Option<PathBuf> {
    debug!("raw URI: {}", uri);

    let request_path = uri.path();

    debug!("raw URI to path: {}", request_path);

    // This is equivalent to checking for hyper::RequestUri::AbsoluteUri
    if !request_path.starts_with("/") {
        debug!("found non-absolute path");
        return None;
    }

    // Trim off the url parameters starting with '?'
    let end = request_path.find('?').unwrap_or(request_path.len());
    let request_path = &request_path[0..end];

    // Convert %-encoding to actual values
    let decoded = percent_decode_str(&request_path);
    let request_path = if let Ok(p) = decoded.decode_utf8() {
        p
    } else {
        debug!("unable to percent-decode URL: {}", request_path);
        // FIXME: Error handling
        return None;
    };

    // Append the requested path to the root directory
    let mut path = root_dir.to_owned();
    if request_path.starts_with('/') {
        path.push(&request_path[1..]);
    } else {
        debug!("found non-absolute path");
        return None;
    }

    debug!("URL · path : {} · {}", uri, path.display());

    Some(path)
}

/// Convert an error to an HTTP error response future, with correct response code.
async fn make_error_response(e: Error) -> Result<Response<Body>> {
    let resp = match e {
        Error::Io(e) => make_io_error_response(e).await?,
        e => make_internal_server_error_response(e).await?,
    };
    Ok(resp)
}

/// Convert an error into a 500 internal server error, and log it.
async fn make_internal_server_error_response(err: Error) -> Result<Response<Body>> {
    log_error_chain(&err);
    let resp = make_error_response_from_code(StatusCode::INTERNAL_SERVER_ERROR).await?;
    Ok(resp)
}

/// Handle the one special io error (file not found) by returning a 404, otherwise
/// return a 500.
async fn make_io_error_response(error: io::Error) -> Result<Response<Body>> {
    let resp = match error.kind() {
        io::ErrorKind::NotFound => {
            debug!("{}", error);
            make_error_response_from_code(StatusCode::NOT_FOUND).await?
        }
        _ => make_internal_server_error_response(Error::Io(error)).await?,
    };
    Ok(resp)
}

/// Make an error response given an HTTP status code.
async fn make_error_response_from_code(status: StatusCode) -> Result<Response<Body>> {
    let body = render_error_html(status)?;
    let resp = html_str_to_response(body, status)?;
    Ok(resp)
}

/// Make an HTTP response from a HTML string.
fn html_str_to_response(body: String, status: StatusCode) -> Result<Response<Body>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CONTENT_TYPE, mime::TEXT_HTML.as_ref())
        .body(Body::from(body))
        .map_err(Error::from)
}

/// A handlebars HTML template
static HTML_TEMPLATE: &str = include_str!("template.html");

/// The data for the handlebars HTML template. Handlebars will use serde to get
/// the data out of the struct and mapped onto the template.
#[derive(Serialize)]
struct HtmlCfg {
    title: String,
    body: String,
}

/// Render an HTML page with handlebars, the template and the configuration data.
fn render_html(cfg: HtmlCfg) -> Result<String> {
    let reg = Handlebars::new();
    let rendered = reg
        .render_template(HTML_TEMPLATE, &cfg)
        .map_err(Error::TemplateRender)?;
    Ok(rendered)
}

/// Render an HTML page from an HTTP status code
fn render_error_html(status: StatusCode) -> Result<String> {
    render_html(HtmlCfg {
        title: format!("{}", status),
        body: String::new(),
    })
}

/// A custom `Result` typedef
pub type Result<T> = std::result::Result<T, Error>;

/// The basic-http-server error type
///
/// This is divided into two types of errors: "semantic" errors and "blanket"
/// errors. Semantic errors are custom to the local application semantics and
/// are usually preferred, since they add context and meaning to the error
/// chain. They don't require boilerplate `From` implementations, but do require
/// `map_err` to create when they have interior `causes`.
///
/// Blanket errors are just wrappers around other types, like `Io(io::Error)`.
/// These are common errors that occur in many places so are easier to code and
/// maintain, since e.g. every occurrence of an I/O error doesn't need to be
/// given local semantics.
///
/// The criteria of when to use which type of error variant, and their pros and
/// cons, aren't obvious.
///
/// These errors use `derive(Display)` from the `derive-more` crate to reduce
/// boilerplate.
#[derive(Debug, Display)]
pub enum Error {
    // blanket "pass-through" error types
    #[display(fmt = "HTTP error")]
    Http(http::Error),

    #[display(fmt = "I/O error")]
    Io(io::Error),

    #[display(fmt = "Hyper error")]
    Hyper(hyper::Error),

    // custom "semantic" error types
    #[display(fmt = "failed to parse IP address")]
    AddrParse(std::net::AddrParseError),

    #[display(fmt = "markdown is not UTF-8")]
    MarkdownUtf8,

    #[display(fmt = "failed to strip prefix in directory listing")]
    StripPrefixInDirList(std::path::StripPrefixError),

    #[display(fmt = "failed to render template")]
    TemplateRender(handlebars::TemplateRenderError),

    #[display(fmt = "failed to convert URL to local file path")]
    UrlToPath,

    #[display(fmt = "formatting error while creating directory listing")]
    WriteInDirList(std::fmt::Error),
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        use Error::*;

        match self {
            Http(e) => Some(e),
            Io(e) => Some(e),
            Hyper(e) => Some(e),
            AddrParse(e) => Some(e),
            MarkdownUtf8 => None,
            StripPrefixInDirList(e) => Some(e),
            TemplateRender(e) => Some(e),
            UrlToPath => None,
            WriteInDirList(e) => Some(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<http::Error> for Error {
    fn from(e: http::Error) -> Error {
        Error::Http(e)
    }
}

impl From<hyper::Error> for Error {
    fn from(e: hyper::Error) -> Error {
        Error::Hyper(e)
    }
}
