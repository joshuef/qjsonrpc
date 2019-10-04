// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use log::debug;
use structopt::{self, StructOpt};

use super::update::update_commander;
use daemonize::Daemonize;
use failure::{Error, Fail, ResultExt};
use futures::{Future, Stream};
use safe_api::SafeAuthenticator;
use slog::{Drain, Logger};
use std::fs::File;
use std::io::prelude::*;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::{ascii, fmt, fs, str};
use tokio::runtime::current_thread::Runtime;

type SharedSafeAuthenticatorHandle = Arc<Mutex<SafeAuthenticator>>;

const SAFE_AUTHD_PID_FILE: &str = "/tmp/safe-authd.pid";
const SAFE_AUTHD_STDOUT_FILE: &str = "/tmp/safe-authd.out";
const SAFE_AUTHD_STDERR_FILE: &str = "/tmp/safe-authd.err";

pub struct PrettyErr<'a>(&'a dyn Fail);
impl<'a> fmt::Display for PrettyErr<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)?;
        let mut x: &dyn Fail = self.0;
        while let Some(cause) = x.cause() {
            f.write_str(": ")?;
            fmt::Display::fmt(&cause, f)?;
            x = cause;
        }
        Ok(())
    }
}

pub trait ErrorExt {
    fn pretty(&self) -> PrettyErr<'_>;
}

impl ErrorExt for Error {
    fn pretty(&self) -> PrettyErr<'_> {
        PrettyErr(self.as_fail())
    }
}

#[derive(StructOpt, Debug)]
/// SAFE Authenticator daemon
#[structopt(raw(global_settings = "&[structopt::clap::AppSettings::ColoredHelp]"))]
enum CmdArgs {
    /// Start the safe-authd daemon
    #[structopt(name = "start")]
    Start {
        /// File to log TLS keys to for debugging
        #[structopt(long = "keylog")]
        keylog: bool,
        /// TLS private key in PEM format
        #[structopt(parse(from_os_str), short = "k", long = "key", requires = "cert")]
        key: Option<PathBuf>,
        /// TLS certificate in PEM format
        #[structopt(parse(from_os_str), short = "c", long = "cert", requires = "key")]
        cert: Option<PathBuf>,
        /// Enable stateless retries
        #[structopt(long = "stateless-retry")]
        stateless_retry: bool,
        /// Address to listen on
        #[structopt(long = "listen", default_value = "127.0.0.1:33000")]
        listen: SocketAddr,
    },
    /// Stop a running safe-authd
    #[structopt(name = "stop")]
    Stop {},
    /// Restart a running safe-authd
    #[structopt(name = "restart")]
    Restart {
        /// Address to listen on
        #[structopt(long = "listen", default_value = "127.0.0.1:33000")]
        listen: SocketAddr,
    },
    /// Update the application to the latest available version
    #[structopt(name = "update")]
    Update {},
}

pub fn run() -> Result<(), String> {
    // Let's first get all the arguments passed in
    let opt = CmdArgs::from_args();
    debug!("Running authd with options: {:?}", opt);

    let decorator = slog_term::PlainSyncDecorator::new(std::io::stderr());
    let drain = slog_term::FullFormat::new(decorator)
        .use_original_order()
        .build()
        .fuse();

    match opt {
        CmdArgs::Update {} => {
            update_commander().map_err(|err| format!("Error performing update: {}", err))
        }
        CmdArgs::Start { listen, .. } => {
            if let Err(e) = start_authd(Logger::root(drain, o!()), listen) {
                Err(format!("{}", e.pretty()))
            } else {
                Ok(())
            }
        }
        CmdArgs::Stop {} => {
            if let Err(e) = stop_authd(Logger::root(drain, o!())) {
                Err(format!("{}", e.pretty()))
            } else {
                Ok(())
            }
        }
        CmdArgs::Restart { listen } => {
            if let Err(e) = restart_authd(Logger::root(drain, o!()), listen) {
                Err(format!("{}", e.pretty()))
            } else {
                Ok(())
            }
        }
    }
}

fn start_authd(log: Logger, listen: SocketAddr) -> Result<(), Error> {
    println!("Starting SAFE Authenticator daemon...");
    let server_config = quinn::ServerConfig {
        transport: Arc::new(quinn::TransportConfig {
            stream_window_uni: 0,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut server_config = quinn::ServerConfigBuilder::new(server_config);
    server_config.protocols(&[quinn::ALPN_QUIC_HTTP]);

    /*if options.keylog {
        server_config.enable_keylog();
    }

    if options.stateless_retry {
        server_config.use_stateless_retry(true);
    }*/

    /*if let (Some(ref key_path), Some(ref cert_path)) = (options.key, options.cert) {
        let key = fs::read(key_path).context("Failed to read private key")?;
        let key = if key_path.extension().map_or(false, |x| x == "der") {
            quinn::PrivateKey::from_der(&key)?
        } else {
            quinn::PrivateKey::from_pem(&key)?
        };
        let cert_chain = fs::read(cert_path).context("Failed to read certificate chain")?;
        let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
            quinn::CertificateChain::from_certs(quinn::Certificate::from_der(&cert_chain))
        } else {
            quinn::CertificateChain::from_pem(&cert_chain)?
        };
        server_config.certificate(cert_chain, key)?;
    } else {*/
    let dirs = directories::ProjectDirs::from("org", "quinn", "quinn-examples").unwrap();
    let path = dirs.data_local_dir();
    let cert_path = path.join("cert.der");
    let key_path = path.join("key.der");
    let (cert, key) = match fs::read(&cert_path).and_then(|x| Ok((x, fs::read(&key_path)?))) {
        Ok(x) => x,
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
            info!(log, "generating self-signed certificate");
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]);
            let key = cert.serialize_private_key_der();
            let cert = cert.serialize_der();
            fs::create_dir_all(&path).context("Failed to create certificate directory")?;
            fs::write(&cert_path, &cert).context("Failed to write certificate")?;
            fs::write(&key_path, &key).context("Failed to write private key")?;
            (cert, key)
        }
        Err(e) => {
            bail!("Failed to read certificate: {}", e);
        }
    };
    let key = quinn::PrivateKey::from_der(&key)?;
    let cert = quinn::Certificate::from_der(&cert)?;
    server_config.certificate(quinn::CertificateChain::from_certs(vec![cert]), key)?;
    //}

    let mut endpoint = quinn::Endpoint::builder();
    endpoint.logger(log.clone());
    endpoint.listen(server_config.build());

    let stdout = File::create(SAFE_AUTHD_STDOUT_FILE).unwrap();
    let stderr = File::create(SAFE_AUTHD_STDERR_FILE).unwrap();

    let daemonize = Daemonize::new()
        .pid_file(SAFE_AUTHD_PID_FILE) // Every method except `new` and `start`
        //.chown_pid_file(true)      // is optional, see `Daemonize` documentation
        .working_directory("/tmp") // for default behaviour.
        //.user("nobody")
        //.group("daemon") // Group name
        //.group(2)        // or group id.
        .umask(0o777) // Set umask, `0o027` by default.
        .stdout(stdout) // Redirect stdout to `/tmp/safe-authd.out`.
        .stderr(stderr) // Redirect stderr to `/tmp/safe-authd.err`.
        .privileged_action(|| "Executed before drop privileges");

    match daemonize.start() {
        Ok(_) => {
            println!("Success, SAFE Authenticator daemonised!");

            let (endpoint_driver, incoming) = {
                let (driver, endpoint, incoming) = endpoint.bind(listen)?;
                info!(log, "listening on {}", endpoint.local_addr()?);
                (driver, incoming)
            };

            let safe_auth_handle: SharedSafeAuthenticatorHandle =
                Arc::new(Mutex::new(SafeAuthenticator::new()));

            let mut runtime = Runtime::new()?;
            runtime.spawn(incoming.for_each(move |conn| {
                handle_connection(safe_auth_handle.clone(), &log, conn);
                Ok(())
            }));
            runtime.block_on(endpoint_driver)?;
        }
        Err(e) => eprintln!("Error, {}", e),
    }

    Ok(())
}

fn stop_authd(_log: Logger) -> Result<(), Error> {
    println!("Stopping SAFE Authenticator daemon...");
    let mut file = File::open(SAFE_AUTHD_PID_FILE)?;
    let mut pid = String::new();
    file.read_to_string(&mut pid)?;
    let output = Command::new("kill").arg("-9").arg(&pid).output()?;

    if output.status.success() {
        io::stdout().write_all(&output.stdout)?;
        println!("Success, safe-authd stopped!");
        Ok(())
    } else {
        io::stdout().write_all(&output.stderr)?;
        bail!("Failed to stop safe-authd daemon");
    }
}

fn restart_authd(log: Logger, listen: SocketAddr) -> Result<(), Error> {
    stop_authd(log.clone())?;
    start_authd(log, listen)?;
    println!("Success, safe-authd restarted!");
    Ok(())
}

fn handle_connection(
    safe_auth_handle: SharedSafeAuthenticatorHandle,
    log: &Logger,
    conn: (
        quinn::ConnectionDriver,
        quinn::Connection,
        quinn::IncomingStreams,
    ),
) {
    let (conn_driver, conn, incoming_streams) = conn;
    let log = log.clone();
    info!(log, "got connection";
          "remote_id" => %conn.remote_id(),
          "address" => %conn.remote_address(),
          "protocol" => conn.protocol().map_or_else(|| "<none>".into(), |x| String::from_utf8_lossy(&x).into_owned()));
    let log2 = log.clone();
    let safe_auth_handle = safe_auth_handle.clone();

    // We ignore errors from the driver because they'll be reported by the `incoming` handler anyway.
    tokio_current_thread::spawn(conn_driver.map_err(|_| ()));

    // Each stream initiated by the client constitutes a new request.
    tokio_current_thread::spawn(
        incoming_streams
            .map_err(move |e| info!(log2, "connection terminated"; "reason" => %e))
            .for_each(move |stream| {
                handle_request(&safe_auth_handle, &log, stream);
                Ok(())
            }),
    );
}

fn handle_request(
    safe_auth_handle: &SharedSafeAuthenticatorHandle,
    log: &Logger,
    stream: quinn::NewStream,
) {
    let (send, recv) = match stream {
        quinn::NewStream::Bi(send, recv) => (send, recv),
        quinn::NewStream::Uni(_) => unreachable!("Disabled by endpoint configuration"),
    };
    let safe_auth_handle = safe_auth_handle.clone();
    let log = log.clone();
    let log2 = log.clone();
    let log3 = log.clone();

    tokio_current_thread::spawn(
        recv.read_to_end(64 * 1024) // Read the request, which must be at most 64KiB
            .map_err(|e| format_err!("Failed reading request: {}", e))
            .and_then(move |(_, req)| {
                let mut escaped = String::new();
                for &x in &req[..] {
                    let part = ascii::escape_default(x).collect::<Vec<_>>();
                    escaped.push_str(str::from_utf8(&part).unwrap());
                }
                info!(log, "Got request");
                // Execute the request
                let resp = process_get(&safe_auth_handle, &req).unwrap_or_else(move |e| {
                    error!(log, "Failed to process request"; "reason" => %e.pretty());
                    // TODO: implement JSON-RPC rather.
                    // Temporarily prefix message with "[AUTHD_ERROR]" to signal error to the caller,
                    // once we have JSON-RPC we can adhere to its format for errors.
                    format!("[AUTHD_ERROR]:SAFE Authenticator: {}", e.pretty())
                        .into_bytes()
                        .into()
                });

                // Write the response
                tokio::io::write_all(send, resp)
                    .map_err(|e| format_err!("Failed to send response: {}", e))
            })
            // Gracefully terminate the stream
            .and_then(|(send, _)| {
                tokio::io::shutdown(send)
                    .map_err(|e| format_err!("Failed to shutdown stream: {}", e))
            })
            .map(move |_| info!(log3, "Request complete"))
            .map_err(move |e| error!(log2, "Request Failed"; "reason" => %e.pretty())),
    )
}

fn process_get(
    safe_auth_handle: &SharedSafeAuthenticatorHandle,
    x: &[u8],
) -> Result<Box<[u8]>, Error> {
    if x.len() < 4 || &x[0..4] != b"GET " {
        bail!("missing GET");
    }
    if x[4..].len() < 2 || &x[x.len() - 2..] != b"\r\n" {
        bail!("missing \\r\\n");
    }
    let x = &x[4..x.len() - 2];
    let end = x.iter().position(|&c| c == b' ').unwrap_or_else(|| x.len());
    let path = str::from_utf8(&x[..end]).context("path is malformed UTF-8")?;
    let req_args: Vec<&str> = path.split("/").collect();

    let safe_auth: &mut SafeAuthenticator = &mut *(safe_auth_handle.lock().unwrap());

    match req_args[1] {
        "login" => {
            if req_args.len() != 4 {
                bail!("Incorrect number of arguments for 'login' action")
            } else {
                println!("Logging in to SAFE account...");
                let secret = req_args[2];
                let password = req_args[3];

                match safe_auth.log_in(secret, password) {
                    Ok(_) => {
                        println!("Logged in successfully");
                        Ok("Logged in successfully!".as_bytes().into())
                    }
                    Err(err) => {
                        println!("Error occurred when trying to log in: {}", err);
                        bail!(err)
                    }
                }
            }
        }
        "logout" => {
            if req_args.len() != 2 {
                bail!("Incorrect number of arguments for 'logout' action")
            } else {
                println!("Logging out...");
                match safe_auth.log_out() {
                    Ok(()) => {
                        println!("Logged out successfully");
                        Ok("Logged out successfully".as_bytes().into())
                    }
                    Err(err) => {
                        println!("Failed to log out: {}", err);
                        bail!(format!("Failed to log out: {}", err))
                    }
                }
            }
        }
        "create" => {
            if req_args.len() != 5 {
                bail!("Incorrect number of arguments for 'create' action")
            } else {
                println!("Creating an account in SAFE...");
                let secret = req_args[2];
                let password = req_args[3];
                let sk = req_args[4];

                match safe_auth.create_acc(sk, secret, password) {
                    Ok(_) => {
                        println!("Account created successfully");
                        Ok("Account created successfully!".as_bytes().into())
                    }
                    Err(err) => {
                        println!("Error occurred when trying to create SAFE account: {}", err);
                        bail!(err)
                    }
                }
            }
        }
        "authorise" => {
            if req_args.len() != 3 {
                bail!("Incorrect number of arguments for 'authorise' action")
            } else {
                println!("Authorising application...");
                let auth_req = req_args[2];

                // TODO: send ntification to user to either allow or deny.
                // TODO: If not end point was reigtered for allowing/denyig reqs then reject it.
                match safe_auth.authorise_app(auth_req /*, allow_callback*/) {
                    Ok(resp) => {
                        println!("Authorisation response sent");
                        Ok(resp.as_bytes().into())
                    }
                    Err(err) => {
                        println!("Failed to authorise: {}", err);
                        bail!(err)
                    }
                }
            }
        }
        "authed-apps" => {
            if req_args.len() != 2 {
                bail!("Incorrect number of arguments for 'authed-apps' action")
            } else {
                println!("Obtaining list of authorised applications...");
                match safe_auth.authed_apps() {
                    Ok(resp) => {
                        println!("List of authorised apps sent");
                        Ok(format!("{:?}", resp).as_bytes().into())
                    }
                    Err(err) => {
                        println!("Failed to get list of authorised apps: {}", err);
                        bail!(err)
                    }
                }
            }
        }
        "revoke" => {
            if req_args.len() != 3 {
                bail!("Incorrect number of arguments for 'revoke' action")
            } else {
                println!("Revoking application...");
                let app_id = req_args[2];

                match safe_auth.revoke_app(app_id) {
                    Ok(()) => {
                        println!("Application revoked successfully");
                        Ok("Application revoked successfully".as_bytes().into())
                    }
                    Err(err) => {
                        println!("Failed to revoke application '{}': {}", app_id, err);
                        bail!(err)
                    }
                }
            }
        }
        other => {
            println!(
                "Action '{}' not supported or unknown by the Authenticator daemon",
                other
            );
            bail!("Action not supported or unknown")
        }
    }
}
