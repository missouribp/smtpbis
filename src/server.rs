use std::collections::BTreeMap;
use std::fmt::Write;

use async_trait::async_trait;
use bytes::BytesMut;

use futures_util::future::{select, Either, FusedFuture};
use tokio::codec::{Framed, FramedParts};
use tokio::prelude::*;

use crate::{command, Command, Command::Base, Command::*};
use crate::{LineCodec, LineError, Reply};

use rustyknife::behaviour::{Intl, Legacy};
use rustyknife::rfc5321::Command::*;
use rustyknife::rfc5321::{ForwardPath, Param, ReversePath};
use rustyknife::types::{Domain, DomainPart};

pub type HandlerResult = Result<Option<Reply>, Option<Reply>>;
pub type EhloKeywords = BTreeMap<String, Option<String>>;
pub type ShutdownSignal = dyn FusedFuture<Output = Result<(), ()>> + Send + Unpin;

#[async_trait]
pub trait Handler {
    type TlsConfig;
    type TlsSession;

    async fn tls_request(&mut self) -> Option<Self::TlsConfig>;
    async fn tls_started(&mut self, session: &Self::TlsSession);

    async fn ehlo(
        &mut self,
        domain: DomainPart,
        initial_keywords: EhloKeywords,
    ) -> Result<(String, EhloKeywords), Reply>;
    async fn helo(&mut self, domain: Domain) -> HandlerResult;
    async fn rset(&mut self);

    async fn mail(&mut self, path: ReversePath, params: Vec<Param>) -> HandlerResult;
    async fn rcpt(&mut self, path: ForwardPath, params: Vec<Param>) -> HandlerResult;

    async fn data_start(&mut self) -> HandlerResult;
    async fn data<S>(&mut self, stream: &mut S) -> Result<Option<Reply>, ServerError>
    where
        S: Stream<Item = Result<BytesMut, LineError>> + Unpin + Send;
    async fn bdat<S>(
        &mut self,
        stream: &mut S,
        size: u64,
        last: bool,
    ) -> Result<Option<Reply>, ServerError>
    where
        S: Stream<Item = Result<BytesMut, LineError>> + Unpin + Send;
}

pub struct Config {
    pub enable_smtputf8: bool,
    pub enable_chunking: bool,
    pub enable_starttls: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enable_smtputf8: true,
            enable_chunking: true,
            enable_starttls: true,
        }
    }
}

pub async fn smtp_server<S, H>(
    socket: &mut S,
    handler: &mut H,
    config: &Config,
    shutdown: &mut ShutdownSignal,
    banner: bool,
) -> Result<LoopExit<H>, ServerError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    H: Handler,
{
    let terminated = shutdown.is_terminated();
    let mut server = InnerServer {
        handler,
        config,
        state: State::Initial,
        shutdown,
        shutdown_on_idle: terminated,
    };

    let res = server.serve(socket, banner).await;
    socket.flush().await?;
    Ok(res?)
}

pub enum LoopExit<H: Handler> {
    Done,
    STARTTLS(H::TlsConfig),
}

#[derive(Debug, PartialEq)]
enum State {
    Initial,
    MAIL,
    RCPT,
    BDAT,
    BDATFAIL,
}

struct InnerServer<'a, H> {
    handler: &'a mut H,
    config: &'a Config,
    state: State,
    shutdown: &'a mut ShutdownSignal,
    shutdown_on_idle: bool,
}

impl<'a, H> InnerServer<'a, H>
where
    H: Handler,
{
    async fn serve<S>(
        &mut self,
        base_socket: &mut S,
        banner: bool,
    ) -> Result<LoopExit<H>, ServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut socket = Framed::new(base_socket, LineCodec::default());

        if banner {
            socket
                .send(Reply::new(220, None, "localhost ESMTP smtpbis 0.1.0"))
                .await?;
        }

        loop {
            let cmd = match self.read_command(&mut socket).await {
                Ok(cmd) => cmd,
                Err(ServerError::SyntaxError(_)) => {
                    socket
                        .send(Reply::syntax_error())
                        .await?;
                    continue;
                }
                Err(ServerError::Shutdown) => {
                    socket.send(Reply::new(421, None, "Shutting down")).await?;
                    return Ok(LoopExit::Done);
                }
                Err(e) => return Err(e),
            };
            println!("command: {:?}", cmd);

            match self.dispatch_command(&mut socket, cmd).await? {
                Some(LoopExit::STARTTLS(tls_config)) => {
                    socket.flush().await?;
                    let FramedParts { io, read_buf, .. } = socket.into_parts();
                    // Absolutely do not allow pipelining past a
                    // STARTTLS command.
                    if !read_buf.is_empty() {
                        return Err(ServerError::Pipelining);
                    }
                    let tls_reply = Reply::new(220, None, "starting TLS").to_string();

                    io.write_all(tls_reply.as_bytes()).await?;
                    return Ok(LoopExit::STARTTLS(tls_config));
                }
                Some(LoopExit::Done) => {
                    return Ok(LoopExit::Done);
                }
                None => {}
            }

            println!("State: {:?}\n", self.state);
        }
    }

    fn shutdown_check(&self) -> Result<(), ServerError> {
        match (self.shutdown_on_idle, &self.state) {
            (true, State::Initial) | (true, State::BDATFAIL) => Err(ServerError::Shutdown),
            _ => Ok(()),
        }
    }

    async fn read_command<S>(&mut self, reader: &mut S) -> Result<Command, ServerError>
    where
        S: Stream<Item = Result<BytesMut, LineError>> + Unpin,
        S: Sink<Reply>,
        ServerError: From<<S as Sink<Reply>>::Error>,
    {
        println!("Waiting for command...");

        self.shutdown_check()?;

        let line = if self.shutdown.is_terminated() {
            reader.next().await
        } else {
            match select(reader.next(), &mut self.shutdown).await {
                Either::Left((cmd, _)) => cmd,
                Either::Right((_, cmd_fut)) => {
                    self.shutdown_on_idle = true;
                    self.shutdown_check()?;
                    cmd_fut.await
                }
            }
        }
        .ok_or(ServerError::EOF)??;

        let parse_res = if self.config.enable_smtputf8 {
            command::<Intl>(&line)
        } else {
            command::<Legacy>(&line)
        };

        match parse_res {
            Err(_) => Err(ServerError::SyntaxError(line)),
            Ok((rem, _)) if !rem.is_empty() => Err(ServerError::SyntaxError(line)),
            Ok((_, cmd)) => Ok(cmd),
        }
    }

    async fn dispatch_command<S>(
        &mut self,
        socket: &mut Framed<&mut S, LineCodec>,
        command: Command,
    ) -> Result<Option<LoopExit<H>>, ServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        match command {
            Base(EHLO(domain)) => {
                socket.send(self.do_ehlo(domain).await?).await?;
            }
            Base(HELO(domain)) => {
                socket.send(self.do_helo(domain).await?).await?;
            }
            Base(MAIL(path, params)) => {
                socket.send(self.do_mail(path, params).await?).await?;
            }
            Base(RCPT(path, params)) => {
                socket.send(self.do_rcpt(path, params).await?).await?;
            }
            Base(DATA) => {
                let reply = self.do_data(socket).await?;
                socket.send(reply).await?;
            }
            Base(QUIT) => {
                socket.send(Reply::new(221, None, "bye")).await?;
                return Ok(Some(LoopExit::Done));
            }
            Base(RSET) => {
                self.state = State::Initial;
                self.handler.rset().await;
                socket.send(Reply::ok()).await?;
            }
            Ext(crate::Ext::STARTTLS) if self.config.enable_starttls => {
                println!("STARTTLS !");

                if let Some(tls_config) = self.handler.tls_request().await {
                    return Ok(Some(LoopExit::STARTTLS(tls_config)));
                } else {
                    socket
                        .send(Reply::not_implemented())
                        .await?;
                }
            }
            Ext(crate::Ext::BDAT(size, last)) if self.config.enable_chunking => {
                let reply = self.do_bdat(socket, size, last).await?;
                socket.send(reply).await?;
            }
            _ => {
                socket
                    .send(Reply::not_implemented())
                    .await?;
            }
        }
        Ok(None)
    }

    async fn do_ehlo(&mut self, domain: DomainPart) -> Result<Reply, ServerError> {
        let mut initial_keywords = EhloKeywords::new();
        for kw in ["PIPELINING", "ENHANCEDSTATUSCODES"].as_ref() {
            initial_keywords.insert((*kw).into(), None);
        }
        if self.config.enable_smtputf8 {
            initial_keywords.insert("SMTPUTF8".into(), None);
        }
        if self.config.enable_chunking {
            initial_keywords.insert("CHUNKING".into(), None);
        }
        if self.config.enable_starttls {
            initial_keywords.insert("STARTTLS".into(), None);
        }

        match self.handler.ehlo(domain, initial_keywords).await {
            Err(reply) => Ok(reply),
            Ok((greeting, keywords)) => {
                assert!(!greeting.contains('\r') && !greeting.contains('\n'));
                let mut reply_text = format!("{}\n", greeting);

                for (kw, value) in keywords {
                    match value {
                        Some(value) => writeln!(reply_text, "{} {}", kw, value).unwrap(),
                        None => writeln!(reply_text, "{}", kw).unwrap(),
                    }
                }
                self.state = State::Initial;
                Ok(Reply::new(250, None, reply_text))
            }
        }
    }

    async fn do_helo(&mut self, domain: Domain) -> Result<Reply, ServerError> {
        Ok(match self.handler.helo(domain).await {
            Ok(reply) => {
                self.state = State::Initial;
                reply.unwrap_or_else(Reply::ok)
            }
            Err(reply) => reply.unwrap_or_else(|| Reply::new(550, None, "refused")),
        })
    }

    async fn do_mail(
        &mut self,
        path: ReversePath,
        params: Vec<Param>,
    ) -> Result<Reply, ServerError> {
        Ok(match self.state {
            State::Initial => match self.handler.mail(path, params).await {
                Ok(reply) => {
                    self.state = State::MAIL;
                    reply.unwrap_or_else(Reply::ok)
                }
                Err(reply) => {
                    reply.unwrap_or_else(|| Reply::new(550, None, "mail transaction refused"))
                }
            },
            _ => Reply::bad_sequence(),
        })
    }

    async fn do_rcpt(
        &mut self,
        path: ForwardPath,
        params: Vec<Param>,
    ) -> Result<Reply, ServerError> {
        Ok(match self.state {
            State::MAIL | State::RCPT => match self.handler.rcpt(path, params).await {
                Ok(reply) => {
                    self.state = State::RCPT;
                    reply.unwrap_or_else(Reply::ok)
                }
                Err(reply) => {
                    reply.unwrap_or_else(|| Reply::new(550, None, "recipient not accepted"))
                }
            },
            _ => Reply::bad_sequence(),
        })
    }

    async fn do_data<S>(&mut self, socket: &mut S) -> Result<Reply, ServerError>
    where
        S: Stream<Item = Result<BytesMut, LineError>> + Unpin + Send,
        S: Sink<Reply>,
        ServerError: From<<S as Sink<Reply>>::Error>,
    {
        Ok(match self.state {
            State::RCPT => match self.handler.data_start().await {
                Ok(reply) => {
                    socket
                        .send(reply.unwrap_or_else(|| Reply::new(354, None, "send data")))
                        .await?;

                    let mut body_stream = read_body_data(socket).fuse();
                    let reply = self.handler.data(&mut body_stream).await?;

                    if !body_stream.is_done() {
                        drop(body_stream);
                        socket
                            .send(reply.unwrap_or_else(|| Reply::new(550, None, "data abort")))
                            .await?;

                        return Err(ServerError::DataAbort);
                    }

                    self.state = State::Initial;
                    reply.unwrap_or_else(Reply::ok)
                }
                Err(reply) => reply.unwrap_or_else(|| Reply::new(550, None, "data not accepted")),
            },
            State::Initial => Reply::no_mail_transaction(),
            State::MAIL => Reply::no_valid_recipients(),
            State::BDAT | State::BDATFAIL => {
                Reply::new(503, None, "BDAT may not be mixed with DATA")
            }
        })
    }

    async fn do_bdat<S>(
        &mut self,
        socket: &mut Framed<S, LineCodec>,
        chunk_size: u64,
        last: bool,
    ) -> Result<Reply, ServerError>
    where
        Framed<S, LineCodec>: Stream<Item = Result<BytesMut, LineError>>
            + Sink<Reply, Error = LineError>
            + Send
            + Unpin,
    {
        Ok(match self.state {
            State::RCPT | State::BDAT => {
                let mut body_stream = read_body_bdat(socket, chunk_size).fuse();

                let reply = self
                    .handler
                    .bdat(&mut body_stream, chunk_size, last)
                    .await
                    .map_err(|e| {
                        self.state = State::BDATFAIL;
                        e
                    })?;

                if !body_stream.is_done() {
                    drop(body_stream);
                    socket
                        .send(reply.unwrap_or_else(|| Reply::new(550, None, "data abort")))
                        .await?;

                    self.state = State::BDATFAIL;
                    return Err(ServerError::DataAbort);
                }

                self.state = if last { State::Initial } else { State::BDAT };
                reply.unwrap_or_else(Reply::ok)
            }
            State::MAIL => Reply::no_valid_recipients(),
            _ => Reply::no_mail_transaction(),
        })
    }
}

#[derive(Debug)]
pub enum ServerError {
    EOF,
    Framing(LineError),
    SyntaxError(BytesMut),
    IO(std::io::Error),
    Pipelining,
    DataAbort,
    Shutdown,
}

impl From<LineError> for ServerError {
    fn from(source: LineError) -> Self {
        match source {
            LineError::IO(e) => Self::IO(e),
            _ => Self::Framing(source),
        }
    }
}

impl From<std::io::Error> for ServerError {
    fn from(err: std::io::Error) -> Self {
        Self::IO(err)
    }
}

#[must_use]
fn read_body_data<'a, S>(source: &'a mut S) -> impl Stream<Item = Result<BytesMut, LineError>> + 'a
where
    S: Stream<Item = Result<BytesMut, LineError>> + Unpin,
{
    source
        .take_while(|res| {
            tokio::future::ready(
                res.as_ref()
                    .map(|line| line.as_ref() != b".\r\n")
                    .unwrap_or(true),
            )
        })
        .map(|res| {
            res.map(|mut line| {
                if line.starts_with(b".") {
                    line.split_to(1);
                }
                line
            })
        })
}

#[must_use]
fn read_body_bdat<'a, S>(
    socket: &'a mut Framed<S, LineCodec>,
    size: u64,
) -> impl Stream<Item = Result<BytesMut, LineError>> + 'a
where
    Framed<S, LineCodec>: Stream<Item = Result<BytesMut, LineError>> + Unpin,
{
    socket.codec_mut().chunking_mode(size);

    socket.take_while(|chunk| {
        let more = match chunk {
            Err(LineError::ChunkingDone) => false,
            _ => true,
        };

        tokio::future::ready(more)
    })
}
