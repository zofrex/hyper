//! HTTP Server
//!
//! A `Server` is created to listen on port, parse HTTP requests, and hand
//! them off to a `Handler`.
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::thread;

use mio::{EventSet, PollOpt};
use rotor::{self, Scope};

pub use self::request::Request;
pub use self::response::Response;

use http::{self, Next};
use net::{Accept, HttpListener, HttpsListener, Ssl, Transport};


mod request;
mod response;
mod message;

/// A Server that can accept incoming network requests.
#[derive(Debug)]
pub struct Server<T: Accept> {
    listener: T,
    keep_alive: bool,
    idle_timeout: Duration,
    max_sockets: usize,
}

impl<T> Server<T> where T: Accept, T::Output: Transport {
    /// Creates a new server with the provided Listener.
    #[inline]
    pub fn new(listener: T) -> Server<T> {
        Server {
            listener: listener,
            keep_alive: true,
            idle_timeout: Duration::from_secs(10),
            max_sockets: 4096,
        }
    }

    /// Enables or disables HTTP keep-alive.
    ///
    /// Default is true.
    pub fn keep_alive(mut self, val: bool) -> Server<T> {
        self.keep_alive = val;
        self
    }

    /// Sets how long an idle connection will be kept before closing.
    ///
    /// Default is 10 seconds.
    pub fn idle_timeout(mut self, val: Duration) -> Server<T> {
        self.idle_timeout = val;
        self
    }

    /// Sets the maximum open sockets for this Server.
    ///
    /// Default is 4096, but most servers can handle much more than this.
    pub fn max_sockets(mut self, val: usize) -> Server<T> {
        self.max_sockets = val;
        self
    }
}

impl Server<HttpListener> {
    /// Creates a new HTTP server listening on the provided address.
    pub fn http(addr: &SocketAddr) -> ::Result<Server<HttpListener>> {
        use ::mio::tcp::TcpListener;
        TcpListener::bind(addr)
            .map(HttpListener)
            .map(Server::new)
            .map_err(From::from)
    }
}


impl<S: Ssl> Server<HttpsListener<S>> {
    /// Creates a new server that will handle `HttpStream`s over SSL.
    ///
    /// You can use any SSL implementation, as long as implements `hyper::net::Ssl`.
    pub fn https(addr: &SocketAddr, ssl: S) -> ::Result<Server<HttpsListener<S>>> {
        HttpsListener::new(addr, ssl)
            .map(Server::new)
            .map_err(From::from)
    }
}


impl<A: Accept + Send + 'static> Server<A> where A::Output: Transport  {
    /// Binds to a socket and starts handling connections.
    pub fn handle<H>(self, factory: H) -> ::Result<Listening>
    where H: HandlerFactory<A::Output> {
        let addr = try!(self.listener.local_addr());
        let (notifier_tx, notifier_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_rx = shutdown.clone();
        let handle = try!(thread::Builder::new().name("hyper-server".to_owned()).spawn(move || {
            let mut config = rotor::Config::new();
            config.slab_capacity(self.max_sockets);
            config.mio().notify_capacity(self.max_sockets);
            let keep_alive = self.keep_alive;
            let mut loop_ = rotor::Loop::new(&config).unwrap();
            loop_.add_machine_with(move |scope| {
                rotor_try!(notifier_tx.send(scope.notifier()));
                rotor_try!(scope.register(&self.listener, EventSet::readable(), PollOpt::level()));
                rotor::Response::ok(ServerFsm::Listener::<A, H>(self.listener, shutdown_rx))
            }).unwrap();
            loop_.run(Context {
                keep_alive: keep_alive,
                factory: factory,
            }).unwrap();
        }));

        let notifier = notifier_rx.recv().unwrap();

        Ok(Listening {
            addr: addr,
            shutdown: (shutdown, notifier),
            handle: Some(handle),
        })
    }
}

struct Context<F> {
    keep_alive: bool,
    factory: F,
}

impl<F: HandlerFactory<T>, T: Transport> http::MessageHandlerFactory<T> for Context<F> {
    type Output = message::Message<F::Output, T>;

    fn create(&mut self, ctrl: http::Control) -> Self::Output {
        message::Message::new(self.factory.create(ctrl))
    }
}

enum ServerFsm<A, H>
where A: Accept,
      A::Output: Transport,
      //F: http::MessageHandlerFactory<A::Output, Output=message::Message<H::Output, A::Output>>,
      H: HandlerFactory<A::Output> {
    Listener(A, Arc<AtomicBool>),
    Conn(http::Conn<A::Output, message::Message<H::Output, A::Output>>)
}

impl<A, H> rotor::Machine for ServerFsm<A, H>
where A: Accept,
      A::Output: Transport,
      H: HandlerFactory<A::Output> {
    type Context = Context<H>;
    type Seed = A::Output;

    fn create(seed: Self::Seed, scope: &mut Scope<Self::Context>) -> rotor::Response<Self, rotor::Void> {
        rotor_try!(scope.register(&seed, EventSet::readable(), PollOpt::level()));
        rotor::Response::ok(
            ServerFsm::Conn(
                http::Conn::new(seed, scope.notifier())
                    .keep_alive(scope.keep_alive)
            )
        )
    }

    fn ready(self, events: EventSet, scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        match self {
            ServerFsm::Listener(listener, rx) => {
                match listener.accept() {
                    Ok(Some(conn)) => {
                        rotor::Response::spawn(ServerFsm::Listener(listener, rx), conn)
                    },
                    Ok(None) => rotor::Response::ok(ServerFsm::Listener(listener, rx)),
                    Err(e) => {
                        error!("listener accept error {}", e);
                        // usually fine, just keep listening
                        rotor::Response::ok(ServerFsm::Listener(listener, rx))
                    }
                }
            },
            ServerFsm::Conn(conn) => {
                match conn.ready(events, scope) {
                    Some((conn, None)) => rotor::Response::ok(ServerFsm::Conn(conn)),
                    Some((conn, Some(dur))) => {
                        rotor::Response::ok(ServerFsm::Conn(conn))
                            .deadline(scope.now() + dur)
                    }
                    None => rotor::Response::done()
                }
            }
        }
    }

    fn spawned(self, _scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        match self {
            ServerFsm::Listener(listener, rx) => {
                match listener.accept() {
                    Ok(Some(conn)) => {
                        rotor::Response::spawn(ServerFsm::Listener(listener, rx), conn)
                    },
                    Ok(None) => rotor::Response::ok(ServerFsm::Listener(listener, rx)),
                    Err(e) => {
                        error!("listener accept error {}", e);
                        // usually fine, just keep listening
                        rotor::Response::ok(ServerFsm::Listener(listener, rx))
                    }
                }
            },
            sock => rotor::Response::ok(sock)
        }

    }

    fn timeout(self, scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        match self {
            ServerFsm::Listener(..) => unimplemented!(),
            ServerFsm::Conn(conn) => {
                match conn.timeout(scope) {
                    Some((conn, None)) => rotor::Response::ok(ServerFsm::Conn(conn)),
                    Some((conn, Some(dur))) => {
                        rotor::Response::ok(ServerFsm::Conn(conn))
                            .deadline(scope.now() + dur)
                    }
                    None => rotor::Response::done()
                }
            }
        }
    }

    fn wakeup(self, scope: &mut Scope<Self::Context>) -> rotor::Response<Self, Self::Seed> {
        match self {
            ServerFsm::Listener(lst, shutdown) => {
                if shutdown.load(Ordering::Acquire) {
                    let _ = scope.deregister(&lst);
                    scope.shutdown_loop();
                    rotor::Response::done()
                } else {
                    rotor::Response::ok(ServerFsm::Listener(lst, shutdown))
                }
            },
            ServerFsm::Conn(conn) => match conn.wakeup(scope) {
                Some((conn, None)) => rotor::Response::ok(ServerFsm::Conn(conn)),
                Some((conn, Some(dur))) => {
                    rotor::Response::ok(ServerFsm::Conn(conn))
                        .deadline(scope.now() + dur)
                }
                None => rotor::Response::done()
            }
        }
    }
}

/// A handle of the running server.
pub struct Listening {
    /// The address this server is listening on.
    pub addr: SocketAddr,
    shutdown: (Arc<AtomicBool>, rotor::Notifier),
    handle: Option<::std::thread::JoinHandle<()>>,
}

impl fmt::Debug for Listening {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Listening")
            .field("addr", &self.addr)
            .finish()
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.handle.take().map(|handle| {
            handle.join().unwrap();
        });
    }
}

impl Listening {
    /// Starts the Server, blocking until it is shutdown.
    pub fn forever(self) {

    }
    /// Stop the server from listening to its socket address.
    pub fn close(self) {
        debug!("closing server");
        self.shutdown.0.store(true, Ordering::Release);
        self.shutdown.1.wakeup().unwrap();
    }
}

/// A trait to react to server events that happen for each message.
///
/// Each event handler returns it's desired `Next` action.
pub trait Handler<T: Transport>: Send + 'static {
    /// This event occurs first, triggering when a `Request` has been parsed.
    fn on_request(&mut self, request: Request) -> Next;
    /// This event occurs each time the `Request` is ready to be read from.
    fn on_request_readable(&mut self, request: &mut http::Decoder<T>) -> Next;
    /// This event occurs after the first time this handled signals `Next::write()`.
    fn on_response(&mut self, response: &mut Response) -> Next;
    /// This event occurs each time the `Response` is ready to be written to.
    fn on_response_writable(&mut self, response: &mut http::Encoder<T>) -> Next;

    /// This event occurs whenever an `Error` occurs outside of the other events.
    ///
    /// This could IO errors while waiting for events, or a timeout, etc.
    fn on_error(&mut self, err: ::Error) -> Next where Self: Sized {
        debug!("default Handler.on_error({:?})", err);
        http::Next::remove()
    }
}


/// Used to create a `Handler` when a new message is received by the server.
pub trait HandlerFactory<T: Transport>: Send + 'static {
    /// The `Handler` to use for the incoming message.
    type Output: Handler<T>;
    /// Creates the associated `Handler`.
    fn create(&mut self, ctrl: http::Control) -> Self::Output;
}

impl<F, H, T> HandlerFactory<T> for F
where F: FnMut(http::Control) -> H + Send + 'static, H: Handler<T>, T: Transport {
    type Output = H;
    fn create(&mut self, ctrl: http::Control) -> H {
        self(ctrl)
    }
}

#[cfg(test)]
mod tests {
    /*
    use header::Headers;
    use method::Method;
    use mock::MockStream;
    use status::StatusCode;
    use uri::RequestUri;

    use super::{Request, Response, Fresh, Handler, Worker};

    #[test]
    fn test_check_continue_default() {
        let mut mock = MockStream::with_input(b"\
            POST /upload HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Expect: 100-continue\r\n\
            Content-Length: 10\r\n\
            \r\n\
            1234567890\
        ");

        fn handle(_: Request, res: Response<Fresh>) {
            res.start().unwrap().end().unwrap();
        }

        Worker::new(handle, Default::default()).handle_connection(&mut mock);
        let cont = b"HTTP/1.1 100 Continue\r\n\r\n";
        assert_eq!(&mock.write[..cont.len()], cont);
        let res = b"HTTP/1.1 200 OK\r\n";
        assert_eq!(&mock.write[cont.len()..cont.len() + res.len()], res);
    }

    #[test]
    fn test_check_continue_reject() {
        struct Reject;
        impl Handler for Reject {
            fn handle(&self, _: Request, res: Response<Fresh>) {
                res.start().unwrap().end().unwrap();
            }

            fn check_continue(&self, _: (&Method, &RequestUri, &Headers)) -> StatusCode {
                StatusCode::ExpectationFailed
            }
        }

        let mut mock = MockStream::with_input(b"\
            POST /upload HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Expect: 100-continue\r\n\
            Content-Length: 10\r\n\
            \r\n\
            1234567890\
        ");

        Worker::new(Reject, Default::default()).handle_connection(&mut mock);
        assert_eq!(mock.write, &b"HTTP/1.1 417 Expectation Failed\r\n\r\n"[..]);
    }
    */
}
