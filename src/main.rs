use std::collections::{HashMap};
use std::convert::TryInto;
use std::env;
use std::error::Error;
use std::sync::{Arc};
use std::net::SocketAddr;

use futures::channel::oneshot;
use futures::SinkExt;

#[macro_use]
extern crate lazy_static;

use regex::{Regex, RegexBuilder};

use tokio;
use tokio::net::{TcpListener, TcpStream};
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::stream::StreamExt;
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, FramedRead, FramedWrite, LinesCodec, LinesCodecError};

const HANDSHAKE_REGEX : &str = r"^please relay (?P<token>\w{64}) for side (?P<side>\w{16})$";
const TOKEN_SIZE : usize = 64;
const SIDE_SIZE : usize = 16;


struct Buddy<T> {
    my_rx: oneshot::Sender<io::ReadHalf<T>>,
    their_rx: oneshot::Receiver<io::ReadHalf<T>>,
    my_done: oneshot::Sender<()>,
    their_done: oneshot::Receiver<()>,
}

impl<T> Buddy<T> {
    fn new_pair() -> (Self, Self) {
	let (rx_one_send, rx_one_recv) = oneshot::channel();
	let (rx_two_send, rx_two_recv) = oneshot::channel();
	let (done_one_send, done_one_recv) = oneshot::channel();
	let (done_two_send, done_two_recv) = oneshot::channel();
	let buddy_one = Self {
	    my_rx: rx_one_send,
	    their_rx: rx_two_recv,
	    my_done: done_one_send,
	    their_done: done_two_recv,
	};
	let buddy_two = Self {
	    my_rx: rx_two_send,
	    their_rx: rx_one_recv,
	    my_done: done_two_send,
	    their_done: done_one_recv,
	};
	(buddy_one, buddy_two)
    }
}


type BuddyChannel<T> = oneshot::Sender<Buddy<T>>;

type Token = Vec<u8>;
type Side = Vec<u8>;

struct Server<T> {
    waiting: HashMap<Token, HashMap<Side, BuddyChannel<T>>>
}

impl<T> Server<T> {
    fn new() -> Self {
	Self {
	    waiting: HashMap::new(),
	}
    }

    fn add_connection(&mut self, token: Token, side: Side, new_ch: BuddyChannel<T>) -> Result<(), Box<dyn Error>> {
	let old_conns = self.waiting.entry(token).or_insert(HashMap::new());
	let old_side = old_conns.keys().find(|k| **k != side).map(|k| k.clone());
	match old_side {
	    Some(old_side) => {
		let old_ch = old_conns.remove(&old_side).unwrap();

		let (old, new) = Buddy::new_pair();
		

		new_ch.send(new).map_err(|_| "couldn't send to new conn")?;
		old_ch.send(old).map_err(|_| "couldn't send to old conn")?;
	    },
	    None => {
		old_conns.insert(side, new_ch);
	    }
	}
	Ok(())
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {

    let addr = env::args().nth(1).unwrap_or_else(|| "[::]:4001".to_string());
    let mut listener = TcpListener::bind(&addr).await?;

    println!("server running on {}", addr);

    let server : Arc<Mutex<Server<TcpStream>>> = Arc::new(Mutex::new(Server::new()));

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;

	let server = Arc::clone(&server);

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
	    match process(server, stream, addr).await {
		Ok(bytes) => {
		    println!("{} closed cleanly after sending {} bytes", addr, bytes)
		},
		Err(e) =>
		    println!("error occurred with {}: {:?}", addr, e)	
	    }
        });
    }
}

async fn process<T>(server: Arc<Mutex<Server<T>>>, stream: T, addr: SocketAddr) -> Result<u64, Box<dyn Error>>
where T: AsyncReadExt + AsyncWriteExt {
    let (my_rx, my_tx) = io::split(stream);
    let mut lines_rx  = FramedRead::new(my_rx, LinesCodec::new());
    let mut lines_tx = FramedWrite::new(my_tx, LinesCodec::new());

    let line = match lines_rx.next().await {
	Some(Ok(line)) => line,
	_ => return Err("bad line".into())
    };

    lazy_static! {
	static ref RE : Regex = RegexBuilder::new(HANDSHAKE_REGEX)
	    .size_limit(10 * (1 << 21))
	    .build()
	    .unwrap();
    }
    let caps = RE.captures(&line).ok_or("bad handshake")?;
    let token = caps["token"].into();
    let side = caps["side"].into();
   
    let (ch, buddy) = oneshot::channel();
    {
    	let mut server = server.lock().await;
    	server.add_connection(token, side, ch)?;
    }

    let buddy = buddy.await.map_err(|_| "redundant")?;
    let my_rx = lines_rx.into_inner();

    buddy.my_rx.send(my_rx).map_err(|_| "couldn't send buddy our rx")?;
    let mut buddy_rx = buddy.their_rx.await?;
    lines_tx.send("ok").await?;

    let mut my_tx = lines_tx.into_inner();

    tokio::select!{
	bytes = io::copy(&mut buddy_rx, &mut my_tx) => {
	    buddy.my_done.send(()).map_err(|_| "couldn't tell buddy we're done")?;
	    Ok(bytes.unwrap_or(0))
	},
	_ = buddy.their_done => {
	    Ok(0)
	}
    }
}
