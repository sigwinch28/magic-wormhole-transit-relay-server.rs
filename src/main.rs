use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::sync::Arc;
//use std::net::SocketAddr;

use futures::channel::oneshot;
use futures::SinkExt;

#[macro_use]
extern crate lazy_static;

use regex::{Regex, RegexBuilder};

use stream_cancel::{StreamExt as CancelStreamExt, TakeUntil, Tripwire};

use tokio::prelude::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::io;
use tokio::io::BufReader;
use tokio::stream::{StreamExt};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec, BytesCodec};

const HANDSHAKE_REGEX_TXT : &str = r"^please relay (?P<token>\w{64}) for side (?P<side>\w{16})$";
//const TOKEN_SIZE : usize = 64;
//const SIDE_SIZE : usize = 16;

lazy_static! {
    static ref HANDSHAKE_REGEX : Regex = RegexBuilder::new(HANDSHAKE_REGEX_TXT)
	.size_limit(10 * (1 << 21))
	.build()
	.unwrap();
}

type Token = Vec<u8>;
type Side = Vec<u8>;

enum Buddy<T> {
    NotHere(oneshot::Receiver<(io::ReadHalf<T>, io::WriteHalf<T>)>),
    Here(oneshot::Sender<(io::ReadHalf<T>, io::WriteHalf<T>)>),
}

struct Server<T> {
    waiting: HashMap<Token, HashMap<Side, oneshot::Sender<(io::ReadHalf<T>, io::WriteHalf<T>)>>>,
}


impl<T> Server<T> {
    fn new() -> Self {
	Self {
	    waiting: HashMap::new(),
	}
    }

    fn add_connection(&mut self, token: Token, side: Side) -> Result<Buddy<T>, Box<dyn Error>> {
	let old_conns = self.waiting.entry(token).or_insert(HashMap::new());
	let old_side = old_conns.keys().find(|k| **k != side).map(|k| k.clone());
	match old_side {
	    Some(old_side) => {
		let old_ch = old_conns.remove(&old_side).unwrap();
		Ok(Buddy::Here(old_ch))
	    },
	    None => {
		let (send, recv) = oneshot::channel();
		old_conns.insert(side, send);
		Ok(Buddy::NotHere(recv))
	    }
	}
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
	    if let Err(e) = process(server, stream).await {
		println!("error occurred with {}: {:?}", addr, e)	
	    }
        });
    }
}

async fn process<T>(server: Arc<Mutex<Server<T>>>, stream: T) -> Result<(), Box<dyn Error>>
where T: AsyncRead + AsyncWrite {
    let (rx, tx) = io::split(stream);
    let rx_buf = BufReader::new(rx);
    //let mut rx_lines = FramedRead::new(rx, LinesCodec::new());

    let mut line = String::new();
    let a = rx_buf.take(1024);
    let b = a.read_line(&mut line).await?;

    let caps = HANDSHAKE_REGEX.captures(&line).ok_or("bad handshake")?;
    let token = caps["token"].into();
    let side = caps["side"].into();

    let buddy;
    {
     	let mut server = server.lock().await;
    	buddy = server.add_connection(token, side)?;
    }

    let (mut my_rx, mut my_tx) = (rx_buf.into_inner(), tx);
    match buddy {
	Buddy::Here(ch) => {
	    ch.send((my_rx, my_tx)).map_err(|_| "couldn't send stream to buddy")?;
	    Ok(())
	},
	Buddy::NotHere(ch) => {
	    let (mut buddy_rx, mut buddy_tx) = ch.await?;
	    my_tx.write("ok\n".as_bytes()).await?;
	    buddy_tx.write("ok\n".as_bytes()).await?;
	    let us_to_buddy = io::copy(&mut my_rx, &mut buddy_tx);
	    let buddy_to_us = io::copy(&mut buddy_rx, &mut my_tx);

	    tokio::try_join!(us_to_buddy, buddy_to_us)?;
	    Ok(())
	}
    }

    // let buddy = buddy.await.map_err(|_| "redundant")?;
    // let (trigger, tripwire) = Tripwire::new();
    // let rx_bytes = FramedRead::new(rx_lines.into_inner(), BytesCodec::new())
    // 	.take_until(tripwire);

    // buddy.my_rx.send(rx_bytes).map_err(|_| "couldn't send buddy our rx")?;
    // let mut buddy_rx = buddy.their_rx.await?;
    // tx_lines.send("ok").await?;

    // let mut my_tx = tx_lines.into_inner();

    // io::copy(&mut buddy_rx, &mut my_tx);
	
    // Ok(0)
}
