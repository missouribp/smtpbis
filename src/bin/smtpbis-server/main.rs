#![warn(rust_2018_idioms)]

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::BytesMut;

use futures_util::future::{select, Either};
use futures_util::pin_mut;
use futures_util::try_future::TryFutureExt;
use tokio::net::{signal, TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::runtime::Runtime;
use tokio::sync::oneshot::Receiver;

use tokio_rustls::rustls::{
    internal::pemfile::{certs, pkcs8_private_keys},
    NoClientAuth, ServerConfig, ServerSession, Session,
};

use rustyknife::rfc5321::{ForwardPath, Param, Path, ReversePath};
use rustyknife::types::{Domain, DomainPart, Mailbox};
use smtpbis::{
    smtp_server, Config, EhloKeywords, Handler, HandlerResult, LineError, Reply, ServerError,
    ShutdownSignal,
};

const CERT: &[u8] = include_bytes!("ssl-cert-snakeoil.pem");
const KEY: &[u8] = include_bytes!("ssl-cert-snakeoil.key");

struct DummyHandler {
    tls_config: Arc<ServerConfig>,
    addr: SocketAddr,
    helo: Option<DomainPart>,
    mail: Option<ReversePath>,
    rcpt: Vec<ForwardPath>,
    body: Vec<u8>,
}

#[async_trait]
impl Handler for DummyHandler {
    async fn tls_request(&mut self) -> Option<Arc<ServerConfig>> {
        Some(self.tls_config.clone())
    }

    async fn tls_started(&mut self, session: &ServerSession) {
        println!(
            "TLS started: {:?}/{:?}",
            session.get_protocol_version(),
            session.get_negotiated_ciphersuite()
        );
        self.reset_tx();
    }

    async fn ehlo(
        &mut self,
        domain: DomainPart,
        mut initial_keywords: EhloKeywords,
    ) -> Result<(String, EhloKeywords), Reply> {
        initial_keywords.insert("DSN".into(), None);
        initial_keywords.insert("8BITMIME".into(), None);
        initial_keywords.insert("SIZE".into(), Some("73400320".into()));

        let greet = format!("hello {} from {}", domain, self.addr);
        self.helo = Some(domain);
        self.reset_tx();

        Ok((greet, initial_keywords))
    }

    async fn helo(&mut self, domain: Domain) -> HandlerResult {
        self.helo = Some(DomainPart::Domain(domain));
        self.reset_tx();

        Ok(None)
    }

    async fn mail(&mut self, path: ReversePath, _params: Vec<Param>) -> HandlerResult {
        println!("Handler MAIL: {:?}", path);

        self.mail = Some(path);
        Ok(None)
    }

    async fn rcpt(&mut self, path: ForwardPath, _params: Vec<Param>) -> HandlerResult {
        println!("Handler RCPT: {:?}", path);
        if let ForwardPath::Path(Path(Mailbox(_, DomainPart::Domain(domain)), _)) = &path {
            if domain.starts_with('z') {
                return Err(None);
            }
        };
        self.rcpt.push(path);
        Ok(None)
    }

    async fn data_start(&mut self) -> HandlerResult {
        println!("Handler DATA start");
        Ok(None)
    }

    async fn data<S>(&mut self, stream: &mut S) -> Result<Option<Reply>, ServerError>
    where
        S: Stream<Item = Result<BytesMut, LineError>> + Unpin + Send,
    {
        println!("Handler DATA read");
        let mut nb_lines: usize = 0;
        self.body.clear();

        while let Some(line) = stream.next().await {
            self.body.extend(line?);
            nb_lines += 1;
        }

        println!("got {} body lines", nb_lines);
        let reply_txt = format!("Received {} bytes in {} lines.", self.body.len(), nb_lines);
        self.reset_tx();

        Ok(Some(Reply::new(250, None, reply_txt)))
    }

    async fn bdat<S>(
        &mut self,
        stream: &mut S,
        _size: u64,
        last: bool,
    ) -> Result<Option<Reply>, ServerError>
    where
        S: Stream<Item = Result<BytesMut, LineError>> + Unpin + Send,
    {
        while let Some(chunk) = stream.next().await {
            self.body.extend(chunk?)
        }
        if last {
            self.reset_tx();
        }

        Ok(None)
    }

    async fn rset(&mut self) {
        self.reset_tx();
    }
}

impl DummyHandler {
    fn reset_tx(&mut self) {
        println!("Reset!");
        self.mail = None;
        self.rcpt.clear();
        self.body.clear();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::new()?;

    rt.block_on(async {
        let (listen_shutdown_tx, listen_shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(listen_loop(listen_shutdown_rx));

        let mut ctrl_c = signal::ctrl_c().unwrap();
        ctrl_c.next().await;
        listen_shutdown_tx.send(()).unwrap();
        println!("Waiting for tasks to finish...");
    });

    rt.shutdown_on_idle();

    Ok(())
}

async fn listen_loop(mut shutdown: Receiver<()>) {
    let mut listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();

    let mut tls_config = ServerConfig::new(NoClientAuth::new());
    let certs = certs(&mut Cursor::new(CERT)).unwrap();
    let key = pkcs8_private_keys(&mut Cursor::new(KEY)).unwrap().remove(0);
    tls_config.set_single_cert(certs, key).unwrap();
    let tls_config = Arc::new(tls_config);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_rx = shutdown_rx.map_err(|_| ()).shared();

    loop {
        let accept = listener.accept();
        pin_mut!(accept);

        match select(accept, &mut shutdown).await {
            Either::Left((listen_res, _)) => {
                let (socket, addr) = listen_res.unwrap();
                let mut shutdown_rx = shutdown_rx.clone();
                let tls_config = tls_config.clone();

                tokio::spawn(async move {
                    serve_smtp(socket, addr, tls_config, &mut shutdown_rx).await
                })
            }
            Either::Right(..) => {
                println!("socket listening loop stopping");
                shutdown_tx.send(()).unwrap();
                break;
            }
        };
    }
}

async fn serve_smtp(
    socket: TcpStream,
    addr: SocketAddr,
    tls_config: Arc<ServerConfig>,
    shutdown: &mut ShutdownSignal,
) {
    let handler = DummyHandler {
        addr,
        tls_config,
        helo: None,
        mail: None,
        rcpt: Vec::new(),
        body: Vec::new(),
    };

    let config = Config::default();
    if let Err(e) = smtp_server(socket, handler, &config, shutdown).await {
        println!("Top level error: {:?}", e);
    }
}
