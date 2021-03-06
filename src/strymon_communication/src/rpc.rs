// Copyright 2017 ETH Zurich. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::{TcpListener, TcpStream, Shutdown, ToSocketAddrs};
use std::marker::PhantomData;
use std::thread;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{Async, Poll};
use futures::future::Future;
use futures::stream::Stream;
use futures::sync::mpsc;
use futures::sync::oneshot;

use Network;
use transport;
use message::MessageBuf;

use serde::ser::Serialize;
use serde::de::DeserializeOwned;

pub trait Request: Serialize + DeserializeOwned {
    type Success: Serialize + DeserializeOwned;
    type Error: Serialize + DeserializeOwned;

    const NAME: &'static str;
}

type RequestId = u32;

#[derive(Copy, Clone)]
#[repr(u8)]
enum Type {
    Request = 0,
    Response = 1,
}

impl Type {
    fn from_u8(num: u8) -> io::Result<Self> {
        match num {
            0 => Ok(Type::Request),
            1 => Ok(Type::Response),
            _ => Err(io::Error::new(ErrorKind::InvalidData, "invalid req/resp type")),
        }
    }
}

pub struct RequestBuf {
    id: RequestId,
    name: String,
    origin: transport::Sender,
    msg: MessageBuf,
}

impl RequestBuf {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn decode<R: Request>(mut self) -> io::Result<(R, Responder<R>)> {
        let payload = self.msg.pop::<R>()?;
        let responder = Responder {
            id: self.id,
            origin: self.origin,
            marker: PhantomData,
        };

        Ok((payload, responder))
    }
}

pub struct Responder<R: Request> {
    id: RequestId,
    origin: transport::Sender,
    marker: PhantomData<R>,
}

impl<R: Request> Responder<R> {
    pub fn respond(self, res: Result<R::Success, R::Error>) {
        let mut msg = MessageBuf::empty();
        msg.push(Type::Response as u8).unwrap();
        msg.push(self.id).unwrap();
        msg.push(res).unwrap();
        self.origin.send(msg)
    }
}

type Pending = oneshot::Sender<MessageBuf>;

#[must_use = "futures do nothing unless polled"]
pub struct Response<R: Request> {
    rx: oneshot::Receiver<MessageBuf>,
    pending: Arc<Mutex<HashMap<RequestId, Pending>>>,
    id: RequestId,
    _request: PhantomData<R>,
}

impl<R: Request> Response<R> {
    pub fn wait_unwrap(self) -> Result<R::Success, R::Error> {
        self.map_err(|e| e.expect("request failed with I/O error")).wait()
    }
}

impl<R: Request> Future for Response<R> {
    type Item = R::Success;
    type Error = Result<<R as Request>::Error, io::Error>;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        match self.rx.poll() {
            Ok(Async::Ready(mut msg)) => {
                // decode the message
                match msg.pop::<Result<R::Success, R::Error>>() {
                    Ok(Ok(success)) => Ok(Async::Ready(success)),
                    Ok(Err(error)) => Err(Ok(error)),
                    Err(err) => Err(Err(io::Error::new(ErrorKind::Other, err))),
                }
            },
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => Err(Err(io::Error::new(ErrorKind::Other, "request canceled"))),
        }
    }
}

impl<R: Request> Drop for Response<R> {
    fn drop(&mut self) {
        // cancel pending response (if not yet completed)
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

#[derive(Clone)]
pub struct Outgoing {
    next_id: Arc<AtomicUsize>,
    pending: Arc<Mutex<HashMap<RequestId, Pending>>>,
    sender: transport::Sender,
}

impl Outgoing {
    fn next_id(&self) -> RequestId {
        self.next_id.fetch_add(1, Ordering::SeqCst) as u32
    }

    pub fn request<R: Request>(&self, r: &R) -> Response<R> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();

        // step 1: create request packet
        let mut msg = MessageBuf::empty();
        msg.push(Type::Request as u8).unwrap();
        msg.push(id).unwrap();
        msg.push(R::NAME).unwrap();
        msg.push::<&R>(r).unwrap();

        // step 2: add completion handle for pending responses
        {
            let mut pending = self.pending.lock().expect("request thread panicked");
            pending.insert(id, tx);
        }

        // step 3: send packet to network
        self.sender.send(msg);

        // step 4: prepare response decoder
        Response {
            rx: rx,
            pending: self.pending.clone(),
            id: id,
            _request: PhantomData,
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Incoming {
    rx: mpsc::UnboundedReceiver<Result<RequestBuf, io::Error>>,
}

impl Stream for Incoming {
    type Item = RequestBuf;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<RequestBuf>, io::Error> {
        transport::poll_receiver(&mut self.rx)
    }
}

struct Resolver {
    incoming: mpsc::UnboundedSender<Result<RequestBuf, io::Error>>,
    pending: Arc<Mutex<HashMap<RequestId, Pending>>>,
    sender: transport::Sender,
    stream: TcpStream,
}

impl Resolver {
    /// decodes a message received on the incoming socket queue.
    fn decode(&mut self, mut msg: MessageBuf) -> io::Result<()> {
        let ty = msg.pop().and_then(Type::from_u8)?;
        let id = msg.pop::<RequestId>()?;
        match ty {
            // if we got a new request, forward it on the queue for incoming
            // requests and create an opaque requestbuf so the receiver can
            // try to decode it
            Type::Request => {
                let name = msg.pop::<String>()?;
                let buf = RequestBuf {
                    id: id,
                    name: name,
                    origin: self.sender.clone(),
                    msg: msg,
                };

                // try to send to receiver
                if self.incoming.unbounded_send(Ok(buf)).is_err() {
                    error!("incoming request queue dropped, ignoring request");
                }
            }
            // if it was a response, we should have a pending response
            // handler waiting - find it and complete the pending request
            Type::Response => {
                let mut pending = self.pending.lock().unwrap();
                let completed = pending
                    .remove(&id)
                    .and_then(move |tx| tx.send(msg).ok())
                    .is_some();

                if !completed {
                    info!("dropping canceled response for {:?}", id);
                }
            }
        }

        Ok(())
    }

    // starts a dispatcher for incoming message and decide if they are
    // incoming requests or responses
    // TODO(swicki): add a timeout which removes old pending responses
    fn dispatch(mut self) {
        thread::spawn(move || {
            loop {
                let res = match MessageBuf::read(&mut self.stream) {
                    // got a full message, try to decode it
                    Ok(Some(message)) => self.decode(message),
                    // remote end closed connection, shut down this thread
                    Ok(None) => break,
                    // error while receiving, signal this to "incoming" queue
                    Err(err) => Err(err),
                };

                // make sure to announce any network errors to client
                if let Err(err) = res {
                    let _ = self.incoming.unbounded_send(Err(err));
                    break;
                }
            }

            drop(self.stream.shutdown(Shutdown::Both));
        });
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Server {
    external: Arc<String>,
    port: u16,
    rx: mpsc::Receiver<io::Result<(Outgoing, Incoming)>>,
}

impl Server {
    // TODO(swicki) could this be merged with network::Listener?
    fn new(network: Network, port: u16) -> io::Result<Self> {
        let sockaddr = ("0.0.0.0", port);
        let listener = TcpListener::bind(&sockaddr)?;
        let external = network.hostname.clone();
        let port = listener.local_addr()?.port();
        let rx = transport::accept(listener, multiplex);

        Ok(Server {
            external: external,
            port: port,
            rx: rx,
        })
    }

    pub fn external_addr(&self) -> (&str, u16) {
        (&*self.external, self.port)
    }
}

impl Stream for Server {
    type Item = (Outgoing, Incoming);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        transport::poll_receiver(&mut self.rx)
    }
}

/// creates a new request dispatcher/multiplexer for each accepted tcp socket
fn multiplex(stream: TcpStream) -> io::Result<(Outgoing, Incoming)> {
    let instream = stream.try_clone()?;
    let outstream = stream;

    let (incoming_tx, incoming_rx) = mpsc::unbounded();
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let sender = transport::Sender::new(outstream);

    let resolver = Resolver {
        pending: pending.clone(),
        sender: sender.clone(),
        incoming: incoming_tx,
        stream: instream,
    };

    let outgoing = Outgoing {
        next_id: Arc::new(AtomicUsize::new(0)),
        pending: pending,
        sender: sender,
    };

    let incoming = Incoming { rx: incoming_rx };

    resolver.dispatch();

    Ok((outgoing, incoming))
}

impl Network {
    pub fn client<E: ToSocketAddrs>(&self,
                                    endpoint: E)
                                    -> io::Result<(Outgoing, Incoming)> {
        multiplex(TcpStream::connect(endpoint)?)
    }

    pub fn server<P: Into<Option<u16>>>(&self, port: P) -> io::Result<Server> {
        Server::new(self.clone(), port.into().unwrap_or(0))
    }
}

fn _assert() {
    fn _is_send<T: Send>() {}
    _is_send::<Incoming>();
    _is_send::<Outgoing>();
    _is_send::<Server>();
}
/*
TODO fix
#[cfg(test)]
mod tests {
    use futures::stream::Stream;
    use reqresp::Request;
    use Network;

    fn assert_io<F: FnOnce() -> ::std::io::Result<()>>(f: F) {
        f().expect("I/O test failed")
    }

    #[derive(Clone, Serialize, Deserialize)]
    struct Ping(i32);
    #[derive(Clone, Serialize, Deserialize)]
    struct Pong(i32);
    impl Request for Ping {
        type Success = Pong;
        type Error = ();

        const NAME: &'static str = "Ping";
    }

    #[test]
    fn simple_ping() {

        assert_io(|| {
            let network = Network::init()?;
            let server = network.server(None)?;

            let (tx, _) = network.client(server.external_addr())?;
            let server = server.take(1).for_each(|(_, rx)| {
                    let handler = rx.take(1).for_each(move |req| {
                            assert_eq!(req.name(), "Ping");
                            let (req, resp) = req.decode::<Ping>().unwrap();
                            resp.respond(Ok(Pong(req.0 + 1)));

                            Ok(())
                        })
                        .map_err(|e| Err(e).unwrap());

                    async::spawn(handler);

                    Ok(())
                })
                .map_err(|e| Err(e).unwrap());

            let done = futures::lazy(move || {
                async::spawn(server);

                tx.request(&Ping(5))
                    .and_then(move |pong| {
                        assert_eq!(pong.0, 6);
                        Ok(())
                    })
                    .map_err(|_| panic!("got ping error"))
            });

            async::finish(done)
        });
    }
}
*/
